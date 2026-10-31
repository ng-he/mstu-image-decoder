# mstu-image-decoder

An [mstu](https://github.com/ng-he/mstu) plugin: encoded video frames in,
JPEG images out, at whatever size and quality you ask for.

```sh
./build.sh
./test.sh
```

Which is what the center repository calls to build and test this one.

## What it does

It takes frames the way
[mstu-media-dumper](https://github.com/ng-he/mstu-media-dumper) does — a
`codec.change` saying what is coming, then the frames themselves — so a source's
`stream.changed` wires straight into it. Every codec mstu names is decoded:
H.264, H.265, VP9, AV1, and JPEG for a camera that already sends stills.

What comes out is one JPEG per image, with the time of the frame it was made
from and the size it ended up. Downstream decodes a JPEG again, which is the
point: a raster frame is megabytes and a JPEG is tens of kilobytes, so nothing
large crosses the pipeline.

## It needs libav

The one plugin here that is not cargo alone. It builds against libavcodec,
libavformat, libavutil and libswscale — decode, scale and the JPEG encode all
come from there, and a hardware decoder arrives later through the same API.

```sh
apt install libavcodec-dev libavformat-dev libswscale-dev   # debian, ubuntu
```

`./test.sh` says so and passes when they are missing, so a fresh clone of the
center repository stays green. An LGPL build is enough: the decoders are in
libavcodec itself, and x264 and x265 are encoders this does not use.

## How it is put together

`images.rs` holds the seam: **frames in, JPEG out, one backend behind it**. The
decode, the scale and the encode are inside that one piece rather than three
stages passing raster between them — so a backend that keeps its pixels on a
GPU can take its place without anything else agreeing on where pixels live.

Decoding is heavy and that is decode's nature, so it does not happen on the
engine's thread.

Three things measured rather than assumed. libav's JPEG encoder reads quality
from `qmin` and `qmax` and ignores `global_quality` on its own. A decoder
holds frames back for reordering, so `drain` exists to ask for them -- and
since that starts the decoder over, whatever comes next has to be a frame it
can start at, as it does after a frame is let go from a full queue: decoding
on without the references a frame was coded against is how a picture turns to
rubbish. And the encoder refuses a timestamp that did not move, so it is fed a
count of its own and the frame's real time rides on the image.

libav writes to stderr on its own, out of order with everything the engine
says; `talk.rs` takes its lines to the host's log instead, levels and all.

Its page shows the latest image and the size the images are actually coming
out at -- not what the settings asked for, since a size larger than the frame
is clamped and both sides are made even.

The picture reaches the page through a file: live readings carry the length of
a bytes field and not the bytes, by the engine's own design, and the desktop
already serves a local file to a plugin's page. So the latest image is written
to `$XDG_RUNTIME_DIR/mstu-preview-<id>.jpg`, named by a live reading, and
written beside and renamed over so a page never catches half an image. A few
writes a second of a small file, and the `preview` setting turns them off.

`image.changed` goes out before every image rather than once per decoder: an
image is a whole file of its own, so anything shaped like the dumper opens one
for each, and wiring that subscription is all it takes to get a folder of
jpegs. Events reach a subscriber at once while frames queue behind them, so
about one image in fifty arrives after the next file was opened and lands in
it, leaving the one before empty; the engine would have to carry events in
order with the data to make that exact.

Two more things the engine asks of a node in the middle, both learned the hard
way. Events published from inside a command handler deadlock, because the
engine holds its event lock while subscribers run -- so `codec.change` only
marks the decoder for starting and the next `process` starts it. And an event
published before anything subscribed is gone, not kept, so whatever wires a
pipeline subscribes before it sets the source going.

Its fixtures are four keyframes of 128 by 96, one per codec, in
`test_samples` with the script that made them.
