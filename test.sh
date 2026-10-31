#!/usr/bin/env bash
# Runs this plugin's tests.
#
# It is the one plugin that needs something installed: libav, which it decodes
# with. Without it there is nothing to test rather than something broken, so
# this says so and passes -- a fresh clone of the center repo stays green.
set -euo pipefail

cd "$(dirname "$0")"

[ "${MSTU_KEEP_LIBCLANG:-}" = "1" ] || unset LIBCLANG_PATH

for lib in libavcodec libavformat libavutil libswscale; do
    if ! pkg-config --exists "$lib" 2>/dev/null; then
        echo "no tests for mstu-image-decoder: $lib is not installed"
        echo "    on debian and ubuntu: apt install libavcodec-dev libavformat-dev libswscale-dev"
        exit 0
    fi
done

# Unformatted code fails too, the same as a failing test would.
cargo fmt -- --check
cargo test "$@"
