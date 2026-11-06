#!/usr/bin/env bash
# Regenerates ghostframe-lib/src/encoder/h264/testdata/openh264_2_6_camera_ltr_128x128.*
# from a real libopenh264 (2.6.x). The committed fixture is what the tests read;
# run this only to change it.
#
#   tools/openh264-fixture/make.sh [path/to/libopenh264.so.8]
#
# Needs gcc, the openh264 headers (wels/codec_api.h), ffmpeg (for the per-frame
# hashes) and the library itself. Nothing here is built into the daemon.
#
# The stream: 128x128, camera usage (screen content silently disables LTR),
# scene-change detection off, LTR mark period 8, feedback with one frame of
# delay, 70 frames, and two scripted recoveries:
#   frame 15  from the first LT, marked by MMCO 6 (the carrier is the picture);
#   frame 42  from an LT marked by MMCO 3 (the picture is the carrier's
#             predecessor). The request names the CARRIER's frame_num.
# The frames before each recovery P are the ones it skips.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
lib="${1:-libopenh264.so.8}"
dest="$repo/ghostframe-lib/src/encoder/h264/testdata"
name=openh264_2_6_camera_ltr_128x128
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

gcc -O1 -Wall -o "$work/enc" "$here/enc.c" -ldl
mkdir -p "$dest"
"$work/enc" "$dest/$name.h264" "$lib" 70 8 1 15,42 2> "$work/log"
grep -E 'openh264 |recovery' "$work/log"
# Per-frame SHA-256 of the full decode (tightly packed I420), one hex digest
# per line, for the tests to compare against.
ffmpeg -v error -i "$dest/$name.h264" -f framehash -hash sha256 - \
    | awk '!/^#/ {gsub(",", ""); print $NF}' > "$dest/$name.sha256"
wc -l "$dest/$name.sha256"
ls -l "$dest/$name.h264"
