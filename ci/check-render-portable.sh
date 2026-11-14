#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# ghostframe-render must stay portable: the web client links it into a wasm
# module that has no threads, no blocking waits, no C libraries and no
# std::time::Instant. The compiler only catches some of that (a std::thread use
# compiles for wasm32-unknown-unknown and panics at run time), so this checks
# the two things that can regress without a build error:
#
#   1. the crate's dependency tree for the wasm target names none of the crates
#      a native-only feature would pull in;
#   2. its source calls nothing that blocks or reads a clock.
#
# `just ci-local`, `just ci-client` and CI's clippy job run it.
set -euo pipefail
cd "$(dirname "$0")/.."

fail() { echo "check-render-portable: $*" >&2; exit 1; }

banned='^(pollster|libc|ash|khronos-egl|ffmpeg-next|ffmpeg-sys-next|tokio|ghostframe-lib|ghostframe-client-gpu|ghostframe-client-h264|ghostframe-client-net) '
deps=$(cargo tree -p ghostframe-render --target wasm32-unknown-unknown -e normal --prefix none)
hits=$(printf '%s\n' "$deps" | grep -E "$banned" | sort -u || true)
if [[ -n "$hits" ]]; then
    printf '%s\n' "$hits" >&2
    fail "the wasm dependency tree of ghostframe-render names a crate it must not link"
fi

# Calls, not mentions: the crate docs say why Instant is absent.
hits=$(grep -rnE 'Instant::now|thread::spawn|std::thread::|pollster::|std::fs::|std::env::' ghostframe-render/src || true)
if [[ -n "$hits" ]]; then
    printf '%s\n' "$hits" >&2
    fail "ghostframe-render reads a clock, spawns or blocks on a thread, or touches the environment"
fi
echo "check-render-portable: ok"
