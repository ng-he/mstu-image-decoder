//! A node that turns encoded video frames into jpeg images.
//!
//! Decoding is heavy and that is decode's nature, so it happens on a thread of
//! its own: `process` only hands frames over and takes images back. What comes
//! out is small enough that nothing large crosses the pipeline.

use std::time::{Duration, Instant};

use mstu_sdk::{
    CommandDescriptor, EventDescriptor, HostContext, Logger, Message, Metadata, PluginDescriptor,
    PluginHandle, ProcessContext, Schema, Slice, Str, Value, Writer, log_debug, log_error,
    log_info, log_warn, message,
};

pub mod images;
pub mod preview;
pub mod schemas;
pub mod talk;
pub mod worker;

use images::Image;
use preview::Preview;
use worker::{Decoder, Job, Options, State};

static METADATA: Metadata = Metadata {
    name: Str::from_static("Image decoder"),
    version: Str::from_static(env!("CARGO_PKG_VERSION")),
};

/// How often readings go out while nothing in particular is happening.
const LIVE_INTERVAL: Duration = Duration::from_millis(250);

/// The window the image rate is measured over.
const RATE_WINDOW: Duration = Duration::from_secs(1);

pub struct ImageDecoderPlugin {
    id: String,
    ctx: *const HostContext,
    log: Logger,

    options: Options,
    decoder: Option<Decoder>,

    /// What the frames are, kept so a change of settings can start a decoder
    /// again without the source having to say it twice.
    codec: String,
    extras: Vec<u8>,

    /// Set when a setting changed, so the next round starts a decoder with it.
    restart: bool,

    rate: Rate,
    beat: Option<Instant>,

    /// The size of the last image made, which is not the size the settings
    /// asked for: it is clamped to the frame and made even.
    size: (u32, u32),

    preview: Preview,
}

impl ImageDecoderPlugin {
    fn new(ctx: *const HostContext, id: String) -> Self {
        let log = Logger::new(ctx, unsafe { METADATA.name.as_str() }, id.as_str());
        let preview = Preview::new(id.as_str());

        // libav would otherwise write to stderr, out of order with the log.
        talk::relay(ctx);

        Self {
            preview,
            ctx,
            log,
            id,
            options: Options::default(),
            decoder: None,
            codec: String::new(),
            extras: Vec::new(),
            restart: false,
            rate: Rate::default(),
            beat: None,
            size: (0, 0),
        }
    }

    /// Pushes what the UI shows; the host keeps the latest.
    fn publish_live(&mut self) {
        let (state, note) = match self.decoder.as_ref() {
            Some(decoder) => decoder.feed.state(),
            None => (State::Idle, String::new()),
        };

        let (frames, made, skipped, backend) = match self.decoder.as_ref() {
            Some(decoder) => {
                let feed = &decoder.feed;
                let load = std::sync::atomic::Ordering::Relaxed;

                (
                    feed.frames.load(load),
                    feed.made.load(load),
                    feed.skipped.load(load),
                    feed.backend(),
                )
            }
            None => (0, 0, 0, "none"),
        };

        let values = [
            Str::from_static(state.as_str()).into(),
            frames.into(),
            made.into(),
            skipped.into(),
            self.rate.sample(made).into(),
            Str::from_static(backend).into(),
            Str::new(note.as_str()).into(),
            (self.size.0 as u64).into(),
            (self.size.1 as u64).into(),
            Str::new(self.preview.path()).into(),
        ];

        unsafe {
            ((*self.ctx).publish_live)(
                Str::new(self.id.as_str()),
                Message {
                    values: Slice::from_raw_parts(values.as_ptr(), values.len()),
                },
            );
        }

        self.beat = Some(Instant::now());
    }

    /// Readings on the slow beat, for a page watching a node that is only
    /// sitting there.
    fn tick(&mut self) {
        let due = match self.beat {
            Some(beat) => beat.elapsed() >= LIVE_INTERVAL,
            None => true,
        };

        if due {
            self.publish_live();
        }
    }

    /// Starts a decoder for what the frames are now, if anything has said.
    fn start(&mut self) {
        self.restart = false;

        if self.codec.is_empty() {
            return;
        }

        // The old thread goes first, so two are never decoding one stream.
        self.decoder = None;

        let log = Logger::new(
            self.ctx,
            unsafe { METADATA.name.as_str() },
            self.id.as_str(),
        );
        let decoder = Decoder::start(self.options, log);

        decoder.feed.give(Job::Codec {
            codec: self.codec.clone(),
            extras: self.extras.clone(),
        });

        self.decoder = Some(decoder);
        self.rate = Rate::default();

        self.publish_live();
    }

    /// Says an image is coming, and what it is.
    ///
    /// Once per image rather than once per decoder: an image is a whole file
    /// of its own, so anything shaped like the dumper opens one for each. It
    /// goes out before the image does, so the file is open when the bytes
    /// arrive.
    fn changed(&mut self) {
        unsafe {
            let writer = ((*self.ctx).new_event)(Str::new(self.id.as_str()), 0) as *mut Writer;

            if writer.is_null() {
                log_warn!(self.log, "the host took no image.changed to fill");
                return;
            }

            let event = &mut *writer;

            let values = [
                Str::new(self.codec.as_str()).into(),
                Str::from_static("jpeg").into(),
                Slice::<u8>::empty().into(),
            ];

            (event.fill)(
                event,
                Message {
                    values: Slice::from_raw_parts(values.as_ptr(), values.len()),
                },
            );

            ((*self.ctx).publish_events)(Str::new(self.id.as_str()), 0);
        }
    }

    /// Hands one frame to the thread.
    fn take(&mut self, input: &Message) {
        let Some(decoder) = self.decoder.as_ref() else {
            return;
        };

        let values = input.values_slice();

        let start_time = values
            .first()
            .and_then(|value| value.get::<u64>())
            .unwrap_or(0);

        let is_sync = values
            .get(1)
            .and_then(|value| value.get::<bool>())
            .unwrap_or(true);

        let Some(bytes) = values.get(2).and_then(|value| value.get::<Slice<u8>>()) else {
            log_warn!(self.log, "a frame with no bytes in it");
            return;
        };

        decoder.feed.give(Job::Frame {
            bytes: unsafe { bytes.as_slice() }.to_vec(),
            start_time,
            is_sync,
        });
    }

    /// Writes one image into the message the engine brought.
    fn hand(&mut self, output: &mut message::Writer, image: &Image) -> Result<(), String> {
        // Before the bytes: whoever takes them has to know one is coming.
        self.changed();

        let payload = (output.reserve)(output, image.bytes.len());

        if payload.is_null() {
            return Err("the message would not take the image".to_string());
        }

        let room = unsafe { std::slice::from_raw_parts_mut(payload, image.bytes.len()) };

        room.copy_from_slice(&image.bytes);

        let bytes = Slice::from_raw_parts(payload, image.bytes.len());

        self.size = (image.width, image.height);
        self.preview.keep(&image.bytes);

        log_debug!(
            self.log,
            "image at {} us, {} by {}",
            image.start_time,
            image.width,
            image.height
        );

        let values = [
            image.start_time.into(),
            (image.width as u64).into(),
            (image.height as u64).into(),
            bytes.into(),
        ];

        (output.fill)(
            output,
            Message {
                values: Slice::from_raw_parts(values.as_ptr(), values.len()),
            },
        );

        self.publish_live();

        Ok(())
    }

    fn process(
        &mut self,
        input: *const Message,
        output: &mut message::Writer,
    ) -> Result<(), String> {
        if self.restart {
            self.start();
        }

        if !input.is_null() {
            self.take(unsafe { &*input });
        }

        // One image a round: the engine calls this once per frame, and there
        // are never more images than frames.
        let ready = self
            .decoder
            .as_ref()
            .and_then(|decoder| decoder.feed.take());

        match ready {
            Some(image) => self.hand(output, &image)?,
            None => self.tick(),
        }

        Ok(())
    }
}

/// Images a second, measured over a window rather than counted from the
/// beginning, so it reads as what is happening now.
#[derive(Default)]
struct Rate {
    at: Option<Instant>,
    made: u64,
    fps: u64,
}

impl Rate {
    fn sample(&mut self, made: u64) -> u64 {
        let Some(at) = self.at else {
            self.at = Some(Instant::now());
            self.made = made;

            return 0;
        };

        let elapsed = at.elapsed();

        if elapsed < RATE_WINDOW {
            return self.fps;
        }

        self.fps = (made.saturating_sub(self.made) * 1000) / elapsed.as_millis().max(1) as u64;
        self.at = Some(Instant::now());
        self.made = made;

        self.fps
    }
}

extern "C" fn metadata() -> &'static Metadata {
    &METADATA
}

extern "C" fn create(ctx: *const HostContext, id: Str) -> PluginHandle {
    let mut plugin = ImageDecoderPlugin::new(ctx, id.to_string());

    log_info!(plugin.log, "created");

    // So a page opening before anything happens has readings to show.
    plugin.publish_live();

    Box::into_raw(Box::new(plugin)) as PluginHandle
}

extern "C" fn id(instance: PluginHandle) -> Str {
    let plugin = unsafe { &*(instance as *mut ImageDecoderPlugin) };
    Str::new(plugin.id.as_str())
}

extern "C" fn release(instance: PluginHandle) {
    unsafe {
        let plugin = Box::from_raw(instance as *mut ImageDecoderPlugin);

        log_info!(plugin.log, "released");

        // Dropping the decoder stops its thread.
        drop(plugin);
    }
}

extern "C" fn process(instance: PluginHandle, p_ctx: *mut ProcessContext) -> bool {
    let plugin = unsafe { &mut *(instance as *mut ImageDecoderPlugin) };
    let context = unsafe { &mut *p_ctx };

    match plugin.process(context.input, unsafe { &mut *context.output }) {
        Ok(()) => true,
        Err(err) => {
            log_error!(plugin.log, "{err}");
            false
        }
    }
}

extern "C" fn process_input_schema() -> *const Schema {
    &schemas::INPUT_SCHEMA
}

extern "C" fn process_output_schema() -> *const Schema {
    &schemas::OUTPUT_SCHEMA
}

extern "C" fn settings_schema() -> *const Schema {
    &schemas::SETTINGS_SCHEMA
}

extern "C" fn settings(_instance: PluginHandle, _output: *mut message::Writer) -> bool {
    true
}

extern "C" fn set_parameter(instance: PluginHandle, field: usize, value: Value) -> bool {
    let plugin = unsafe { &mut *(instance as *mut ImageDecoderPlugin) };

    // Nothing starts here: the settings arrive one by one while the pipeline
    // is stopped, and a decoder per field would be eight of them. The next
    // round of `process` picks the change up.
    match field {
        0 => plugin.options.wanted.width = number(value) as u32,
        1 => plugin.options.wanted.height = number(value) as u32,
        2 => plugin.options.wanted.quality = number(value).clamp(1, 100) as u32,
        3 => plugin.options.rate = value.get::<f64>().unwrap_or(0.0).max(0.0),
        4 => plugin.options.keyframes_only = value.get::<bool>().unwrap_or(false),
        5 => plugin.options.wanted.min_change = number(value).min(100) as u32,
        6 => plugin.options.heartbeat = number(value) as u32,

        8 => plugin.preview.set(value.get::<bool>().unwrap_or(true)),

        7 => {
            let named = value
                .get::<Str>()
                .map(|text| unsafe { text.as_str() }.to_string())
                .unwrap_or_default();

            if !matches!(named.as_str(), "" | "auto" | "software") {
                log_warn!(plugin.log, "no {named} decoder here, using software");
            }
        }

        _ => {
            log_warn!(plugin.log, "ignored unknown parameter {field}");
            return true;
        }
    }

    plugin.restart = true;
    true
}

fn number(value: Value) -> u64 {
    value.get::<u64>().unwrap_or(0)
}

/// UI folder, resolved by the host against the plugin library directory.
extern "C" fn ui() -> Str {
    Str::from_static("mstu-image-decoder-ui")
}

extern "C" fn live_schema() -> *const Schema {
    &schemas::LIVE_SCHEMA
}

extern "C" fn events() -> Slice<EventDescriptor> {
    mstu_sdk::slice!(schemas::EVENTS)
}

extern "C" fn commands() -> Slice<CommandDescriptor> {
    mstu_sdk::slice!(schemas::COMMANDS)
}

extern "C" fn invoke(instance: PluginHandle, command: usize, input: *const Message) -> bool {
    let plugin = unsafe { &mut *(instance as *mut ImageDecoderPlugin) };

    if command != 0 {
        log_warn!(plugin.log, "no such command {command}");
        return false;
    }

    if input.is_null() {
        log_error!(plugin.log, "codec.change with nothing in it");
        return false;
    }

    let values = unsafe { &*input }.values_slice();

    let Some(codec) = values.first().and_then(|value| value.get::<Str>()) else {
        log_error!(plugin.log, "codec.change needs a codec");
        return false;
    };

    plugin.codec = unsafe { codec.as_str() }.to_string();
    plugin.extras = values
        .get(1)
        .and_then(|value| value.get::<Slice<u8>>())
        .map(|bytes| unsafe { bytes.as_slice() }.to_vec())
        .unwrap_or_default();

    // Not started here. The engine holds its event lock while a command
    // runs, and starting publishes image.changed, which would wait on the
    // lock the caller is holding: the next round of `process` does it.
    plugin.restart = true;
    true
}

static PLUGIN: PluginDescriptor = PluginDescriptor {
    metadata,
    create,
    id,
    release,

    process_input_schema,
    process_output_schema,
    settings_schema,

    settings,
    set_parameter,

    ui,

    process,

    events,
    commands,
    invoke,

    live_schema,
};

#[unsafe(no_mangle)]
pub extern "C" fn plugin_descriptor() -> *const PluginDescriptor {
    &PLUGIN
}

#[cfg(test)]
mod tests {
    use core::panic;
    use std::cell::RefCell;
    use std::thread;

    use mstu_sdk::{LogLevel, Writer};

    use super::*;

    /// An image as the host caught it: time, size, and the jpeg.
    type Caught = (u64, u32, u32, Vec<u8>);

    extern "C" fn log_nothing(_: LogLevel, _: Str, _: Str) {}

    extern "C" fn log_disabled(_: LogLevel) -> bool {
        false
    }

    thread_local! {
        static IMAGES: RefCell<Vec<Caught>> = const { RefCell::new(Vec::new()) };
        static LIVE: RefCell<Vec<(String, u64, u64, u64)>> = const { RefCell::new(Vec::new()) };
        static RESERVED: RefCell<Vec<Vec<u8>>> = const { RefCell::new(Vec::new()) };
    }

    /// Memory of the host's, which the message may point at.
    extern "C" fn hold(_w: *mut Writer, len: usize) -> *mut u8 {
        RESERVED.with(|reserved| {
            let mut reserved = reserved.borrow_mut();

            reserved.push(vec![0u8; len]);
            reserved
                .last_mut()
                .expect("what was just put there")
                .as_mut_ptr()
        })
    }

    extern "C" fn out_fill(_w: *mut Writer, message: Message) -> bool {
        let values = message.values_slice();

        let number = |at: usize| {
            values
                .get(at)
                .and_then(|value| value.get::<u64>())
                .unwrap_or(0)
        };

        let bytes = values
            .get(3)
            .and_then(|value| value.get::<Slice<u8>>())
            .map(|slice| unsafe { slice.as_slice() }.to_vec())
            .unwrap_or_default();

        IMAGES.with(|images| {
            images
                .borrow_mut()
                .push((number(0), number(1) as u32, number(2) as u32, bytes))
        });

        true
    }

    extern "C" fn no_event(_: Str, _: usize) -> *const message::Writer {
        std::ptr::null()
    }

    extern "C" fn no_publish(_: Str, _: usize) {}

    extern "C" fn publish_live(_: Str, snapshot: Message) {
        let values = snapshot.values_slice();

        let state = values
            .first()
            .and_then(|value| value.get::<Str>())
            .map(|text| unsafe { text.as_str() }.to_string())
            .unwrap_or_default();

        let number = |at: usize| {
            values
                .get(at)
                .and_then(|value| value.get::<u64>())
                .unwrap_or(0)
        };

        LIVE.with(|live| {
            live.borrow_mut()
                .push((state, number(1), number(2), number(3)))
        });
    }

    /// Leaked on purpose: the engine's context outlives every plugin, and a
    /// test that panics leaves a decoding thread that would otherwise log
    /// through a context that had gone.
    fn host() -> &'static HostContext {
        Box::leak(Box::new(HostContext {
            logger: log_nothing,
            log_enabled: log_disabled,
            new_event: no_event,
            publish_events: no_publish,
            publish_live,
        }))
    }

    fn writer() -> Writer {
        Writer {
            _engine_data: std::ptr::null_mut(),
            fill: out_fill,
            reserve: hold,
        }
    }

    fn images() -> Vec<Caught> {
        IMAGES.with(|images| images.borrow().clone())
    }

    fn live() -> Vec<(String, u64, u64, u64)> {
        LIVE.with(|live| live.borrow().clone())
    }

    fn keyframe() -> Vec<u8> {
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/test_samples/h264.bin"
        ))
        .expect("the h264 keyframe")
    }

    /// Hands one frame over, the way the engine hands one over.
    fn send(handle: PluginHandle, bytes: &[u8], start_time: u64, is_sync: bool) {
        let values = [
            start_time.into(),
            is_sync.into(),
            Slice::from_raw_parts(bytes.as_ptr(), bytes.len()).into(),
        ];

        let input = Message {
            values: Slice::from_raw_parts(values.as_ptr(), values.len()),
        };

        let mut out = writer();
        let mut context = ProcessContext {
            input: &input,
            output: &mut out,
        };

        assert!(
            process(handle, &mut context),
            "a frame must not fail a node"
        );
    }

    /// Rounds with nothing coming in, which is how an image already made
    /// finds its way out.
    fn pump(handle: PluginHandle, within: Duration, done: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + within;
        let mut out = writer();

        while Instant::now() < deadline {
            let mut context = ProcessContext {
                input: std::ptr::null(),
                output: &mut out,
            };

            assert!(process(handle, &mut context));

            if done() {
                return true;
            }

            thread::sleep(Duration::from_millis(5));
        }

        done()
    }

    fn told(handle: PluginHandle, codec: &str, extras: &[u8]) {
        let values = [
            Str::new(codec).into(),
            Slice::from_raw_parts(extras.as_ptr(), extras.len()).into(),
        ];

        let input = Message {
            values: Slice::from_raw_parts(values.as_ptr(), values.len()),
        };

        assert!(invoke(handle, 0, &input), "codec.change");
    }

    /// A plugin with a codec named and the images sized.
    fn decoding(ctx: &'static HostContext, settings: &[(usize, Value)]) -> PluginHandle {
        let handle = create(ctx, Str::from_static("TEST"));

        for (field, value) in settings {
            assert!(set_parameter(handle, *field, *value), "field {field}");
        }

        told(handle, "h264", &[]);
        handle
    }

    fn measure(jpeg: &[u8]) -> (u32, u32) {
        assert_eq!(jpeg[..3], [0xff, 0xd8, 0xff], "the start of a jpeg");

        let mut at = 2;

        while at + 9 < jpeg.len() {
            if jpeg[at] != 0xff {
                at += 1;
                continue;
            }

            let marker = jpeg[at + 1];
            let length = u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]) as usize;

            if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
                let height = u16::from_be_bytes([jpeg[at + 5], jpeg[at + 6]]) as u32;
                let width = u16::from_be_bytes([jpeg[at + 7], jpeg[at + 8]]) as u32;

                return (width, height);
            }

            at += 2 + length;
        }

        panic!("no frame header in the jpeg");
    }

    #[test]
    fn a_frame_in_is_an_image_out() {
        let handle = decoding(host(), &[(0, 64u64.into())]);

        send(handle, &keyframe(), 1_234, true);

        assert!(
            pump(handle, Duration::from_secs(5), || !images().is_empty()),
            "an image should have come out: {:?}",
            live().last()
        );

        let (start_time, width, height, jpeg) = images().remove(0);

        assert_eq!(start_time, 1_234, "the frame's own time");
        assert_eq!((width, height), (64, 48), "sized as the settings asked");
        assert_eq!(measure(&jpeg), (64, 48), "and the jpeg agrees");

        release(handle);
    }

    #[test]
    fn nothing_is_decoded_before_anything_says_what_it_is() {
        let handle = create(host(), Str::from_static("TEST"));

        send(handle, &keyframe(), 0, true);

        assert!(!pump(handle, Duration::from_millis(400), || {
            !images().is_empty()
        }));

        assert!(
            live().iter().all(|(state, ..)| state == "idle"),
            "{:?}",
            live().last()
        );

        release(handle);
    }

    /// A stream joined mid-flight decodes into rubbish until a key frame.
    #[test]
    fn frames_before_the_first_key_frame_are_let_go() {
        let handle = decoding(host(), &[]);

        send(handle, &keyframe(), 0, false);

        assert!(!pump(handle, Duration::from_millis(400), || {
            !images().is_empty()
        }));

        send(handle, &keyframe(), 10, true);

        assert!(pump(handle, Duration::from_secs(5), || !images().is_empty()));
        assert_eq!(images()[0].0, 10, "the key frame, not the one before it");

        release(handle);
    }

    #[test]
    fn keyframes_only_takes_nothing_else() {
        let handle = decoding(host(), &[(4, true.into())]);

        send(handle, &keyframe(), 0, true);
        assert!(pump(handle, Duration::from_secs(5), || !images().is_empty()));

        send(handle, &keyframe(), 10, false);

        assert!(
            !pump(handle, Duration::from_millis(400), || images().len() > 1),
            "a frame that is no key frame should not have been decoded"
        );

        release(handle);
    }

    /// Asking the decoder for what it holds starts it over, so what comes
    /// after a quiet spell has to be a frame it can start at: anything else
    /// decodes against references that are gone.
    #[test]
    fn after_a_quiet_spell_it_waits_for_a_key_frame() {
        let handle = decoding(host(), &[]);

        send(handle, &keyframe(), 0, true);
        assert!(pump(handle, Duration::from_secs(5), || !images().is_empty()));

        // Long enough for the decoder to be asked for what it held.
        pump(handle, Duration::from_millis(600), || false);

        send(handle, &keyframe(), 10, false);

        assert!(
            !pump(handle, Duration::from_millis(400), || images().len() > 1),
            "a frame that is no key frame should not have been decoded"
        );

        send(handle, &keyframe(), 20, true);

        assert!(pump(handle, Duration::from_secs(5), || images().len() > 1));
        assert_eq!(images()[1].0, 20);

        release(handle);
    }

    /// Two frames of the same picture: the second is not worth an image.
    #[test]
    fn a_picture_that_did_not_change_makes_no_second_image() {
        let handle = decoding(host(), &[(5, 50u64.into())]);

        send(handle, &keyframe(), 0, true);
        assert!(pump(handle, Duration::from_secs(5), || !images().is_empty()));

        send(handle, &keyframe(), 10, true);

        assert!(
            !pump(handle, Duration::from_millis(500), || images().len() > 1),
            "the same picture twice is one image"
        );

        release(handle);
    }

    /// The rate holds images back however fast the frames arrive.
    #[test]
    fn one_image_a_second_is_one_image_a_second() {
        let handle = decoding(host(), &[(3, 1.0f64.into())]);

        send(handle, &keyframe(), 0, true);
        assert!(pump(handle, Duration::from_secs(5), || !images().is_empty()));

        send(handle, &keyframe(), 10, true);

        assert!(
            !pump(handle, Duration::from_millis(400), || images().len() > 1),
            "the second image was not due yet"
        );

        release(handle);
    }

    /// Settings arrive one at a time, and the decoder starts once with all of
    /// them rather than once per field.
    #[test]
    fn a_setting_changed_starts_the_decoder_again() {
        let handle = decoding(host(), &[(0, 64u64.into())]);

        send(handle, &keyframe(), 0, true);
        assert!(pump(handle, Duration::from_secs(5), || !images().is_empty()));

        assert!(set_parameter(handle, 0, 32u64.into()));
        send(handle, &keyframe(), 10, true);

        assert!(pump(handle, Duration::from_secs(5), || images().len() > 1));
        assert_eq!(images()[1].1, 32, "made at the new size");

        release(handle);
    }
}
