#!/usr/bin/env bash
# Builds this plugin. The library lands in target/release, where the center
# repo's collect.sh looks for it.
#
# It needs libavcodec, libavformat, libavutil and libswscale to build against:
# decode, scale and the jpeg encode all come from libav. No libavdevice and no
# libavfilter.
set -euo pipefail

cd "$(dirname "$0")"

# A cross compiler's libclang (an esp32 toolchain's, say) reports 32 bit
# pointers, and the bindings generator refuses to work against it. Unless one
# is named on purpose, use whatever clang the system has.
[ "${MSTU_KEEP_LIBCLANG:-}" = "1" ] || unset LIBCLANG_PATH

cargo build --release
