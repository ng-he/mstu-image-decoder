//! Frames in, JPEGs out.
//!
//! The seam goes around the whole chain -- decode, scale, encode -- rather
//! than between its stages, so a backend that keeps its pixels on a GPU can
//! take this one's place without anything in between agreeing on where the
//! pixels live.

use std::collections::VecDeque;

use ffmpeg_next as ff;

/// A JPEG, and the time of the frame it was made from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub start_time: u64,
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
}

/// What the settings ask of the images.
#[derive(Debug, Clone, Copy)]
pub struct Wanted {
    /// Zero means whatever the frame is, and a size larger than the frame is
    /// clamped: this never upscales.
    pub width: u32,
    pub height: u32,

    /// 1 to 100.
    pub quality: u32,

    /// How much of the picture must change before another image is worth
    /// making, 0 to 100. Zero makes one of everything.
    ///
    /// The policy lives here because only this side has the pixels; what to
    /// do about time -- a rate, a heartbeat -- is the caller's business.
    pub min_change: u32,
}

impl Default for Wanted {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            quality: 80,
            min_change: 0,
        }
    }
}

pub trait Images: Send {
    /// Takes one encoded frame, at its time in microseconds.
    fn push(&mut self, bytes: &[u8], start_time: u64) -> Result<(), String>;

    /// A JPEG that is finished, if one is.
    fn pop(&mut self) -> Option<Image>;

    /// Lets the next image through however like the last one it is, which is
    /// how a caller keeps a quiet stream from going silent altogether.
    fn force(&mut self);

    /// Images not made because the picture had not changed enough.
    fn unchanged(&self) -> u64;

    /// Asks for the frames the decoder is still holding, which it keeps back
    /// to allow for reordering.
    ///
    /// This starts the decoder over, so only call it where the next frame
    /// will be one a decoder can start at: nothing else would decode against
    /// the references it just let go.
    fn drain(&mut self) -> Result<(), String>;

    fn backend(&self) -> &'static str;
}

/// The backend for a codec, with the parameter sets the stream declared.
pub fn of(codec: &str, extras: &[u8], wanted: Wanted) -> Result<Box<dyn Images>, String> {
    Ok(Box::new(Software::new(codec, extras, wanted)?))
}

/// What libav calls the codecs mstu names.
fn id_of(codec: &str) -> Option<ff::codec::Id> {
    match codec {
        "h264" | "avc" => Some(ff::codec::Id::H264),
        "h265" | "hevc" => Some(ff::codec::Id::HEVC),
        "vp9" => Some(ff::codec::Id::VP9),
        "av1" => Some(ff::codec::Id::AV1),
        "jpeg" | "mjpeg" => Some(ff::codec::Id::MJPEG),
        _ => None,
    }
}

/// The size to make the image, given the size the frame came out at.
fn target(frame: (u32, u32), wanted: Wanted) -> (u32, u32) {
    let (source_width, source_height) = frame;

    let (width, height) = match (wanted.width, wanted.height) {
        (0, 0) => (source_width, source_height),
        (width, 0) => (width, source_height * width / source_width.max(1)),
        (0, height) => (source_width * height / source_height.max(1), height),
        (width, height) => (width, height),
    };

    // Never upscale, and keep both sides even: a jpeg of 4:2:0 has half
    // resolution chroma, so an odd side has nowhere to put it.
    let even = |side: u32, limit: u32| (side.min(limit) & !1).max(2);

    (even(width, source_width), even(height, source_height))
}

/// A thumbnail is this many samples on a side: enough to tell a picture that
/// moved from one that only gained sensor noise.
const SIGNATURE: usize = 8;

/// The luma of a frame, sampled down to a thumbnail.
fn signature(frame: &ff::frame::Video) -> [u8; SIGNATURE * SIGNATURE] {
    let mut thumbnail = [0u8; SIGNATURE * SIGNATURE];

    let luma = frame.data(0);
    let stride = frame.stride(0);
    let (width, height) = (frame.width() as usize, frame.height() as usize);

    if luma.is_empty() || width == 0 || height == 0 {
        return thumbnail;
    }

    for row in 0..SIGNATURE {
        for column in 0..SIGNATURE {
            let y = row * height / SIGNATURE;
            let x = column * width / SIGNATURE;

            thumbnail[row * SIGNATURE + column] = luma[y * stride + x];
        }
    }

    thumbnail
}

/// How far apart two thumbnails are, as a percentage of the whole range.
fn change(before: &[u8; SIGNATURE * SIGNATURE], after: &[u8; SIGNATURE * SIGNATURE]) -> u32 {
    let total: u32 = before
        .iter()
        .zip(after.iter())
        .map(|(before, after)| before.abs_diff(*after) as u32)
        .sum();

    total * 100 / (before.len() as u32 * u8::MAX as u32)
}

/// libav's quality is a scale of 2 to 31, the other way about from ours.
fn qscale(quality: u32) -> i32 {
    const BEST: i32 = 2;
    const WORST: i32 = 31;

    let quality = quality.clamp(1, 100) as i32;

    WORST - (quality * (WORST - BEST) / 100)
}

struct Software {
    decoder: ff::decoder::Video,
    chain: Option<Chain>,
    wanted: Wanted,

    /// The parameter sets, which go in front of the first frame rather than
    /// into the decoder as extradata: a stream carries them that way anyway.
    extras: Vec<u8>,
    primed: bool,

    ready: VecDeque<Image>,

    /// A thumbnail of the last image made, and whether the next one goes out
    /// whatever it looks like.
    last: Option<[u8; SIGNATURE * SIGNATURE]>,
    forced: bool,
    unchanged: u64,
}

/// libav's contexts are raw pointers, so Rust will not move them between
/// threads on its own; one thread owns this at a time, which is all libav asks.
unsafe impl Send for Software {}

impl Software {
    fn new(codec: &str, extras: &[u8], wanted: Wanted) -> Result<Self, String> {
        ff::init().map_err(|error| error.to_string())?;

        let id = id_of(codec).ok_or_else(|| format!("'{codec}' is not a codec this decodes"))?;

        let found = ff::decoder::find(id)
            .ok_or_else(|| format!("this build of libav has no {codec} decoder"))?;

        let decoder = ff::codec::context::Context::new_with_codec(found)
            .decoder()
            .video()
            .map_err(|error| format!("no {codec} decoder: {error}"))?;

        Ok(Self {
            decoder,
            chain: None,
            wanted,
            extras: extras.to_vec(),
            primed: extras.is_empty(),
            ready: VecDeque::new(),
            last: None,
            forced: false,
            unchanged: 0,
        })
    }

    /// Everything the decoder has finished with, encoded and put by.
    fn collect(&mut self) -> Result<(), String> {
        let mut frame = ff::frame::Video::empty();

        while self.decoder.receive_frame(&mut frame).is_ok() {
            // Measured on the frame rather than on the image, so a picture
            // that has not changed costs no encode either.
            let signature = signature(&frame);

            if self.alike(&signature) {
                self.unchanged += 1;
                continue;
            }

            let image = self.encode(&frame)?;

            self.last = Some(signature);
            self.forced = false;
            self.ready.push_back(image);
        }

        Ok(())
    }

    /// Whether this picture is too like the last one to be worth an image.
    fn alike(&self, signature: &[u8; SIGNATURE * SIGNATURE]) -> bool {
        if self.forced || self.wanted.min_change == 0 {
            return false;
        }

        let Some(last) = self.last.as_ref() else {
            return false;
        };

        change(last, signature) < self.wanted.min_change
    }

    fn encode(&mut self, frame: &ff::frame::Video) -> Result<Image, String> {
        let size = target((frame.width(), frame.height()), self.wanted);

        // A stream may change size mid-flight, which is a new scaler and a
        // new encoder.
        if self.chain.as_ref().map(Chain::size) != Some(size) {
            self.chain = Some(Chain::new(frame, size, self.wanted.quality)?);
        }

        let chain = self.chain.as_mut().expect("the chain was just built");
        let start_time = frame.timestamp().unwrap_or(0).max(0) as u64;

        chain.run(frame, start_time)
    }
}

impl Images for Software {
    fn push(&mut self, bytes: &[u8], start_time: u64) -> Result<(), String> {
        let mut packet = if self.primed {
            ff::Packet::copy(bytes)
        } else {
            let mut whole = Vec::with_capacity(self.extras.len() + bytes.len());

            whole.extend_from_slice(&self.extras);
            whole.extend_from_slice(bytes);
            self.primed = true;

            ff::Packet::copy(&whole)
        };

        packet.set_pts(Some(start_time as i64));
        packet.set_dts(Some(start_time as i64));

        self.decoder
            .send_packet(&packet)
            .map_err(|error| format!("the frame was refused: {error}"))?;

        self.collect()
    }

    fn pop(&mut self) -> Option<Image> {
        self.ready.pop_front()
    }

    fn drain(&mut self) -> Result<(), String> {
        self.decoder
            .send_eof()
            .map_err(|error| format!("the decoder would not close: {error}"))?;

        self.collect()?;

        // Past the end of a stream the decoder answers nothing more, so it is
        // reset and the parameter sets go in again with the next frame.
        self.decoder.flush();
        self.primed = self.extras.is_empty();

        Ok(())
    }

    fn force(&mut self) {
        self.forced = true;
    }

    fn unchanged(&self) -> u64 {
        self.unchanged
    }

    fn backend(&self) -> &'static str {
        "software"
    }
}

/// The scaler and the jpeg encoder for one geometry.
struct Chain {
    scaler: ff::software::scaling::Context,
    encoder: ff::encoder::video::Encoder,
    width: u32,
    height: u32,

    /// What the encoder is told the time is. A frame's own time may stand
    /// still -- a file that loops, a camera with a stuck clock -- and an
    /// encoder refuses a timestamp that did not move; the real time rides on
    /// the image instead.
    counted: i64,
}

impl Chain {
    fn new(frame: &ff::frame::Video, size: (u32, u32), quality: u32) -> Result<Self, String> {
        let (width, height) = size;

        // Jpeg is full range, which is what the j in this format means.
        let scaler = ff::software::scaling::Context::get(
            frame.format(),
            frame.width(),
            frame.height(),
            ff::format::Pixel::YUVJ420P,
            width,
            height,
            ff::software::scaling::Flags::BILINEAR,
        )
        .map_err(|error| format!("no scaler for this frame: {error}"))?;

        let found = ff::encoder::find(ff::codec::Id::MJPEG)
            .ok_or("this build of libav has no jpeg encoder")?;

        let mut encoder = ff::codec::context::Context::new_with_codec(found)
            .encoder()
            .video()
            .map_err(|error| format!("no jpeg encoder: {error}"))?;

        encoder.set_width(width);
        encoder.set_height(height);
        encoder.set_format(ff::format::Pixel::YUVJ420P);
        encoder.set_time_base(ff::Rational::new(1, 1_000_000));

        // Jpeg has no bitrate to hit, so quality is a quantizer scale. The
        // encoder reads it from qmin and qmax: global_quality on its own
        // leaves every image the same size, measured.
        let q = qscale(quality);

        encoder.set_flags(ff::codec::Flags::QSCALE);
        encoder.set_quality((q * ff::ffi::FF_QP2LAMBDA) as usize);
        encoder.set_qmin(q);
        encoder.set_qmax(q);

        let encoder = encoder
            .open_as(found)
            .map_err(|error| format!("the jpeg encoder would not open: {error}"))?;

        Ok(Self {
            scaler,
            encoder,
            width,
            height,
            counted: 0,
        })
    }

    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn run(&mut self, frame: &ff::frame::Video, start_time: u64) -> Result<Image, String> {
        let mut scaled = ff::frame::Video::empty();

        self.scaler
            .run(frame, &mut scaled)
            .map_err(|error| format!("the frame would not scale: {error}"))?;

        scaled.set_pts(Some(self.counted));
        self.counted += 1;

        self.encoder
            .send_frame(&scaled)
            .map_err(|error| format!("the frame was not encoded: {error}"))?;

        let mut packet = ff::Packet::empty();
        let mut bytes = Vec::new();

        // One frame is one jpeg, but the loop keeps the encoder empty either
        // way.
        while self.encoder.receive_packet(&mut packet).is_ok() {
            if let Some(data) = packet.data() {
                bytes.extend_from_slice(data);
            }
        }

        if bytes.is_empty() {
            return Err("the encoder gave back no jpeg".to_string());
        }

        Ok(Image {
            start_time,
            width: self.width,
            height: self.height,
            bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use core::panic;

    use super::*;

    /// The keyframes beside this crate, 128 by 96 each.
    const SIZE: (u32, u32) = (128, 96);

    fn keyframe(codec: &str) -> Vec<u8> {
        let path = format!("{}/test_samples/{codec}.bin", env!("CARGO_MANIFEST_DIR"));

        std::fs::read(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
    }

    /// One image out of one keyframe, at the time it was given.
    fn image(codec: &str, wanted: Wanted, start_time: u64) -> Image {
        let mut images = of(codec, &[], wanted).expect("a backend");

        images
            .push(&keyframe(codec), start_time)
            .expect("the frame");
        images.drain().expect("what the decoder held");
        images.pop().expect("an image")
    }

    /// Width and height as the jpeg's own frame header gives them, so the
    /// test reads what a decoder downstream would read.
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

            // The frame headers, which carry the size; the tables in between
            // share the range and do not.
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
    fn every_codec_we_define_turns_a_keyframe_into_a_jpeg() {
        for codec in ["h264", "h265", "vp9", "av1"] {
            let image = image(codec, Wanted::default(), 0);

            assert_eq!(measure(&image.bytes), SIZE, "{codec}");
            assert_eq!((image.width, image.height), SIZE, "{codec}");
            assert!(
                image.bytes.len() > 500,
                "{codec}: {} bytes",
                image.bytes.len()
            );
        }
    }

    #[test]
    fn the_time_of_the_frame_comes_through_with_it() {
        assert_eq!(
            image("h264", Wanted::default(), 1_234_567).start_time,
            1_234_567
        );
    }

    #[test]
    fn the_image_is_made_at_the_size_asked_for() {
        let wanted = Wanted {
            width: 64,
            height: 48,
            ..Wanted::default()
        };

        assert_eq!(measure(&image("h264", wanted, 0).bytes), (64, 48));
    }

    /// One side given, the other follows the frame's shape.
    #[test]
    fn a_size_of_one_side_keeps_the_aspect() {
        let wanted = Wanted {
            width: 64,
            ..Wanted::default()
        };

        assert_eq!(measure(&image("h264", wanted, 0).bytes), (64, 48));
    }

    #[test]
    fn a_size_larger_than_the_frame_is_clamped_rather_than_blown_up() {
        let wanted = Wanted {
            width: 1920,
            height: 1080,
            ..Wanted::default()
        };

        assert_eq!(measure(&image("h264", wanted, 0).bytes), SIZE);
    }

    #[test]
    fn quality_is_the_size_of_the_jpeg() {
        let small = Wanted {
            quality: 5,
            ..Wanted::default()
        };

        let large = Wanted {
            quality: 95,
            ..Wanted::default()
        };

        let small = image("h264", small, 0).bytes.len();
        let large = image("h264", large, 0).bytes.len();

        assert!(small < large, "{small} bytes at 5, {large} at 95");
    }

    /// The parameter sets may arrive beside the stream rather than in it.
    #[test]
    fn the_sets_given_go_in_front_of_the_first_frame() {
        let whole = keyframe("h264");
        let units = mstu_media::nalu::annexb_ranges(&whole);

        // Everything but the last unit is the sets; the last is the picture.
        let (sets, picture) = whole.split_at(units.last().expect("units").start - 4);

        let mut images = of("h264", sets, Wanted::default()).expect("a backend");

        images.push(picture, 0).expect("the frame");
        images.drain().expect("what the decoder held");

        let image = images.pop().expect("an image");

        assert_eq!(measure(&image.bytes), SIZE);
    }

    /// Two frames may carry the same time -- a file that loops, a camera with
    /// a stuck clock -- and the encoder refuses a timestamp that did not move.
    #[test]
    fn two_frames_at_the_same_time_are_two_images() {
        let mut images = of("h264", &[], Wanted::default()).expect("a backend");

        for _ in 0..2 {
            images.push(&keyframe("h264"), 9_000).expect("the frame");
            images.drain().expect("what the decoder held");
        }

        assert_eq!(
            std::iter::from_fn(|| images.pop()).count(),
            2,
            "both frames should have come out"
        );
    }

    #[test]
    fn a_codec_this_does_not_decode_is_refused() {
        assert!(of("prores", &[], Wanted::default()).is_err());
        assert!(of("", &[], Wanted::default()).is_err());
    }

    #[test]
    fn the_target_size_is_even_and_never_larger_than_the_frame() {
        let wanted = |width, height| Wanted {
            width,
            height,
            ..Wanted::default()
        };

        assert_eq!(target((1920, 1080), wanted(0, 0)), (1920, 1080));
        assert_eq!(target((1920, 1080), wanted(640, 0)), (640, 360));
        assert_eq!(target((1920, 1080), wanted(0, 360)), (640, 360));
        assert_eq!(
            target((1920, 1080), wanted(641, 361)),
            (640, 360),
            "even sides"
        );
        assert_eq!(target((64, 48), wanted(1920, 1080)), (64, 48), "no upscale");
        assert_eq!(
            target((64, 48), wanted(1, 1)),
            (2, 2),
            "nothing smaller than this"
        );
    }

    #[test]
    fn quality_runs_the_other_way_from_libav_s_scale() {
        assert_eq!(qscale(100), 2, "the best libav offers");
        assert!(qscale(1) > qscale(50) && qscale(50) > qscale(100));
        assert_eq!(qscale(0), qscale(1), "clamped rather than out of range");
        assert_eq!(qscale(1000), qscale(100));
    }
}
