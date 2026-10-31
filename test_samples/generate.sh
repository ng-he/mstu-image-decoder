#!/usr/bin/env bash
# Builds the keyframes the tests decode, and leaves them beside this script.
#
# One keyframe per codec, tiny on purpose: enough to prove a frame goes in and
# a jpeg comes out. H.264 and H.265 are Annex-B as they come; VP9 and AV1 have
# no start codes, so they are carried in IVF and the headers are cut off.
set -euo pipefail

cd "$(dirname "$0")"

source() { ffmpeg -hide_banner -loglevel error -f lavfi -i "testsrc2=size=128x96:rate=1" -frames:v 1 "$@"; }

source -c:v libx264 -preset ultrafast -f h264 -y h264.bin
source -c:v libx265 -preset ultrafast -x265-params log-level=none -f hevc -y h265.bin

# IVF is a 32 byte file header, then 12 bytes in front of each frame.
for codec in vp9 av1; do
    case $codec in
        vp9) source -c:v libvpx-vp9 -f ivf -y "$codec.ivf" ;;
        av1) source -c:v libaom-av1 -cpu-used 8 -f ivf -y "$codec.ivf" ;;
    esac

    tail -c +45 "$codec.ivf" > "$codec.bin"
    rm "$codec.ivf"
done

ls -l ./*.bin
