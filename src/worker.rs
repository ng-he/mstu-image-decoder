//! The thread that decodes.
//!
//! `process` runs inside the engine's async runtime, so decoding there would
//! stall every other node on it. This side does the work; the two pass jobs
//! one way and images the other through queues, and nothing on the host is
//! touched from here.

use std::collections::VecDeque;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use mstu_sdk::{Logger, log_debug, log_error, log_info};

use crate::images::{self, Image, Images, Wanted};

/// How long the thread waits on an empty queue before looking around, which
/// bounds how soon it notices it is no longer wanted.
const IDLE: Duration = Duration::from_millis(100);

/// Nothing new has arrived for this long, so the frames the decoder is
/// holding are asked for: a stream of one frame every few seconds should not
/// sit a frame behind.
const SETTLE: Duration = Duration::from_millis(400);

/// Jobs waiting for the thread, and images waiting for the pipeline.
const DEPTH: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Decoding,
    Failed,
}

impl State {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Decoding => "decoding",
            Self::Failed => "failed",
        }
    }
}

/// What the engine's side hands over.
pub enum Job {
    /// What the frames that follow are, and the parameter sets for them.
    Codec { codec: String, extras: Vec<u8> },

    Frame {
        bytes: Vec<u8>,
        start_time: u64,
        is_sync: bool,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub wanted: Wanted,

    /// Images a second. Zero makes one of every frame.
    pub rate: f64,

    pub keyframes_only: bool,

    /// Seconds after which an image goes out however like the last it is.
    /// Zero leaves a still picture silent.
    pub heartbeat: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            wanted: Wanted::default(),
            rate: 0.0,
            keyframes_only: false,
            heartbeat: 0,
        }
    }
}

/// What the two sides share.
pub struct Feed {
    jobs: Mutex<VecDeque<Job>>,
    arrived: Condvar,

    images: Mutex<VecDeque<Image>>,
    said: Mutex<(State, String)>,
    backend: Mutex<&'static str>,

    pub frames: AtomicU64,
    pub made: AtomicU64,
    pub skipped: AtomicU64,

    /// A frame was let go, so the ones after it decode against references
    /// that are no longer there.
    gap: AtomicBool,

    stopping: AtomicBool,
}

impl Feed {
    fn new() -> Self {
        Self {
            jobs: Mutex::new(VecDeque::new()),
            arrived: Condvar::new(),
            images: Mutex::new(VecDeque::new()),
            said: Mutex::new((State::Idle, String::new())),
            backend: Mutex::new("none"),
            frames: AtomicU64::new(0),
            made: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            gap: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
        }
    }

    /// Hands a job to the thread. The oldest frame goes when the queue is
    /// full: a live stream carries on whether or not the decoder keeps up.
    pub fn give(&self, job: Job) {
        let mut jobs = self.jobs.lock().expect("the jobs");

        while jobs.len() >= DEPTH {
            let oldest = jobs
                .iter()
                .position(|held| matches!(held, Job::Frame { .. }));

            match oldest {
                Some(at) => {
                    jobs.remove(at);
                    self.skipped.fetch_add(1, Ordering::Relaxed);
                    self.gap.store(true, Ordering::Relaxed);
                }

                // Nothing in there but news, which is not worth dropping.
                None => break,
            }
        }

        jobs.push_back(job);
        self.arrived.notify_one();
    }

    /// An image for the pipeline, if one is ready.
    pub fn take(&self) -> Option<Image> {
        self.images.lock().expect("the images").pop_front()
    }

    pub fn state(&self) -> (State, String) {
        self.said.lock().expect("the state").clone()
    }

    pub fn backend(&self) -> &'static str {
        *self.backend.lock().expect("the backend")
    }

    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
        self.arrived.notify_all();
    }

    fn say(&self, state: State, note: &str) {
        *self.said.lock().expect("the state") = (state, note.to_string());
    }

    fn stopping(&self) -> bool {
        self.stopping.load(Ordering::Relaxed)
    }

    /// Whether a frame has been let go since this was last asked, which the
    /// decoder has to start over from a key frame after.
    fn gapped(&self) -> bool {
        self.gap.swap(false, Ordering::Relaxed)
    }

    /// The next job, or nothing once `IDLE` has passed with none.
    fn next(&self) -> Option<Job> {
        let mut jobs = self.jobs.lock().expect("the jobs");

        if jobs.is_empty() {
            let (held, _) = self
                .arrived
                .wait_timeout(jobs, IDLE)
                .expect("the jobs to come back");

            jobs = held;
        }

        jobs.pop_front()
    }

    /// Puts an image by, dropping the oldest if nothing is collecting.
    fn made(&self, image: Image) {
        let mut images = self.images.lock().expect("the images");

        while images.len() >= DEPTH {
            images.pop_front();
            self.skipped.fetch_add(1, Ordering::Relaxed);
        }

        images.push_back(image);
        self.made.fetch_add(1, Ordering::Relaxed);
    }
}

/// The logger, over on the decoding thread.
///
/// Sound because the host's log hook is the only thing called from here, and
/// that one is a thread safe sink.
struct Log(Logger);

unsafe impl Send for Log {}

impl Deref for Log {
    type Target = Logger;

    fn deref(&self) -> &Logger {
        &self.0
    }
}

/// A decoder running on its own thread.
pub struct Decoder {
    pub feed: Arc<Feed>,
    working: Option<JoinHandle<()>>,
}

impl Decoder {
    pub fn start(options: Options, log: Logger) -> Self {
        let feed = Arc::new(Feed::new());

        let working = {
            let feed = Arc::clone(&feed);
            let log = Log(log);

            thread::spawn(move || Work::new(&feed, options, &log).run())
        };

        Self {
            feed,
            working: Some(working),
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.feed.stop();

        // The thread looks around every idle timeout, so this waits a tenth
        // of a second at worst.
        if let Some(working) = self.working.take() {
            let _ = working.join();
        }
    }
}

/// The thread's own state, which nothing else sees.
struct Work<'a> {
    feed: &'a Arc<Feed>,
    log: &'a Log,
    options: Options,

    backend: Option<Box<dyn Images>>,

    /// Nothing is decoded until a frame a decoder can start at arrives.
    wants_sync: bool,

    /// When the last image went out, and when the last frame came in.
    sent: Option<Instant>,
    took: Instant,

    /// Whether the decoder has been asked for what it holds since the last
    /// frame arrived.
    settled: bool,
}

impl<'a> Work<'a> {
    fn new(feed: &'a Arc<Feed>, options: Options, log: &'a Log) -> Self {
        Self {
            feed,
            log,
            options,
            backend: None,
            wants_sync: true,
            sent: None,
            took: Instant::now(),
            settled: true,
        }
    }

    fn run(mut self) {
        while !self.feed.stopping() {
            match self.feed.next() {
                Some(Job::Codec { codec, extras }) => self.codec(&codec, &extras),
                Some(Job::Frame {
                    bytes,
                    start_time,
                    is_sync,
                }) => self.frame(&bytes, start_time, is_sync),

                // Nothing came: ask the decoder for what it is holding.
                None => self.quiet(),
            }
        }

        log_debug!(self.log, "the decoder stopped");
    }

    fn codec(&mut self, codec: &str, extras: &[u8]) {
        match images::of(codec, extras, self.options.wanted) {
            Ok(backend) => {
                log_info!(
                    self.log,
                    "decoding {codec} with {} ({} byte(s) of parameter sets)",
                    backend.backend(),
                    extras.len()
                );

                *self.feed.backend.lock().expect("the backend") = backend.backend();
                self.feed.say(State::Decoding, "");

                self.backend = Some(backend);
                self.wants_sync = true;
            }

            Err(error) => {
                log_error!(self.log, "{error}");

                self.feed.say(State::Failed, &error);
                self.backend = None;
            }
        }
    }

    fn frame(&mut self, bytes: &[u8], start_time: u64, is_sync: bool) {
        if self.backend.is_none() {
            // Frames before anything said what they are.
            self.feed.skipped.fetch_add(1, Ordering::Relaxed);
            return;
        }

        self.feed.frames.fetch_add(1, Ordering::Relaxed);

        // Frames let go while this was busy leave the ones after them without
        // the references they were coded against.
        if self.feed.gapped() {
            self.wants_sync = true;
        }

        // A stream joined mid-flight decodes into rubbish until a key frame,
        // and only the frames a decoder may start at are wanted in that mode.
        let skip = (self.wants_sync || self.options.keyframes_only) && !is_sync;

        if skip {
            self.feed.skipped.fetch_add(1, Ordering::Relaxed);
            return;
        }

        self.wants_sync = false;
        self.beat();

        let backend = self.backend.as_mut().expect("a backend");

        if let Err(error) = backend.push(bytes, start_time) {
            log_error!(self.log, "{error}");
            self.feed.say(State::Failed, &error);

            return;
        }

        self.took = Instant::now();
        self.settled = false;

        // A key frame stands on its own, so what it decoded to can be asked
        // for at once rather than waiting behind the next frame.
        if self.options.keyframes_only && self.drain().is_err() {
            return;
        }

        self.collect();
    }

    /// Nothing has arrived for a while: take what the decoder held back.
    fn quiet(&mut self) {
        if self.settled || self.took.elapsed() < SETTLE {
            return;
        }

        self.settled = true;

        if self.drain().is_ok() {
            self.collect();
        }
    }

    fn drain(&mut self) -> Result<(), String> {
        let backend = self.backend.as_mut().ok_or("no decoder")?;

        let drained = backend.drain().inspect_err(|error| {
            log_error!(self.log, "{error}");
            self.feed.say(State::Failed, error);
        });

        // Asking the decoder for what it held starts it over, so whatever
        // comes next has to be a frame it can start at.
        self.wants_sync = true;

        drained
    }

    /// Hands on the images the decoder made, as often as the rate allows.
    fn collect(&mut self) {
        let Some(backend) = self.backend.as_mut() else {
            return;
        };

        let mut made = Vec::new();

        while let Some(image) = backend.pop() {
            made.push(image);
        }

        for image in made {
            if self.too_soon() {
                self.feed.skipped.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            log_debug!(
                self.log,
                "image at {} us, {} by {}, {} byte(s)",
                image.start_time,
                image.width,
                image.height,
                image.bytes.len()
            );

            self.sent = Some(Instant::now());
            self.feed.made(image);
        }
    }

    /// Whether an image would come sooner than the rate asked for.
    fn too_soon(&self) -> bool {
        if self.options.rate <= 0.0 {
            return false;
        }

        let apart = Duration::from_secs_f64(1.0 / self.options.rate);

        self.sent
            .map(|sent| sent.elapsed() < apart)
            .unwrap_or(false)
    }

    /// Lets one image through unchanged or not, so a still picture does not
    /// go silent.
    fn beat(&mut self) {
        if self.options.heartbeat == 0 {
            return;
        }

        let due = Duration::from_secs(self.options.heartbeat as u64);
        let overdue = self.sent.map(|sent| sent.elapsed() >= due).unwrap_or(true);

        if let Some(backend) = self.backend.as_mut().filter(|_| overdue) {
            backend.force();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_let_go_is_said_once() {
        let feed = Feed::new();

        assert!(!feed.gapped(), "nothing has been let go yet");

        for at in 0..DEPTH + 2 {
            feed.give(Job::Frame {
                bytes: vec![at as u8],
                start_time: at as u64,
                is_sync: false,
            });
        }

        assert_eq!(feed.skipped.load(Ordering::Relaxed), 2);
        assert!(feed.gapped(), "and the decoder has to be told");
        assert!(!feed.gapped(), "but only until it has been");
    }

    /// News is not a frame, and must survive a queue that is overflowing.
    #[test]
    fn what_the_decoder_must_hear_is_never_let_go() {
        let feed = Feed::new();

        feed.give(Job::Codec {
            codec: "h264".to_string(),
            extras: vec![],
        });

        for at in 0..DEPTH + 4 {
            feed.give(Job::Frame {
                bytes: vec![at as u8],
                start_time: at as u64,
                is_sync: false,
            });
        }

        assert!(matches!(feed.next(), Some(Job::Codec { .. })));
    }
}
