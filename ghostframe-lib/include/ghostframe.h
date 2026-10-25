#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>



/**
 * Initial bitrate seed. Sized for a typical broadband first-paint
 * burst (2 Mbps); the estimator will adapt within a few hundred ms.
 */
#define BweWrapper_INITIAL_BPS 2000000

#define DECODE_ERROR_SIZE 5

#define ERR_PAYLOAD_TOO_SHORT 1

#define ERR_COUNT_OUT_OF_RANGE 2

#define ERR_THIN_UNCACHED_PALETTE 3

#define ERR_BUNDLED_TRUNCATED 4

#define ERR_INDEX_OOB 5

#define ERR_RLE_OVERSHOOT 6

#define ERR_RLE_UNDERSHOOT 7

#define DISPLAY_INFO_SIZE 11

#define DISPLAY_MODE_SIZE 5

/**
 * Inline capacity for the coverage list per key.
 * Sized for typical bundled-PalRle and single-pass-Cdf53 cases without
 * heap allocation.
 */
#define COVERAGE_INLINE_CAPACITY 8

/**
 * Coverage map capacity. Sized to hold all in-flight passes for a full
 * 1920×1080 Cdf53 emission cycle with headroom: 2040 tiles × 14 passes ×
 * 2 frames of RTT ≈ 57,120 entries. Round up to 60,000.
 *
 * At 5,000 the map evicts entries faster than ACKs arrive, causing
 * `ack_entry_miss` on more than half of all ACK datagrams and permanently
 * blocking `tile_fully_acked` from returning true.
 *
 * TODO(m3.3e): make this a function of `(cols × rows × max_passes × rtt_frames)`
 * instead of a fixed const; at 4K @ 60 fps the worst case grows ~8× further.
 */
#define FRAGMENT_COVERAGE_CAPACITY 60000

#define FEC_GROUP_SIZE_K 10

#define FEC_PARITY_PER_GROUP_R 1

#define PARITY_INTERLEAVE_OFFSET (uint32_t)(2 * FEC_GROUP_SIZE_K)

#define END_OF_STREAM_PARITY_FLUSH_MS 5

#define CACHE_CAPACITY 32768

/**
 * ACK envelope wire-format version. Bumped 0x04 → 0x06 in 2026-09-17 when
 * entries switched from naming a tile-pass to naming a transmission.
 * 0x05 is skipped: it is `TILE_NACK_ENVELOPE`, and an ACK batch landing in
 * the NACK handler would be silently dropped by the wrong decoder — a
 * collision this project has already shipped once and caught late.
 *
 * Old clients/servers are not wire-compatible with new; both sides ship in
 * lockstep, and the bumped byte makes a stale binary fail loud with
 * `WrongMsgType` rather than mis-parse.
 */
#define ACK_BATCH_MSG_TYPE 6

/**
 * Maximum number of *fresh* entries the client packs into one batch
 * before flushing (mirrors `MAX_ACK_ENTRIES` in ack.ts). Used for the
 * roundtrip-size tests; the wire-acceptance cap is below.
 */
#define MAX_FRESH_ENTRIES_PER_BATCH 64

/**
 * Trailing overlap count the client appends to each batch (mirrors
 * `ACK_OVERLAP_COUNT` in ack.ts). A single dropped ACK batch
 * therefore needs ACK_OVERLAP_COUNT + 1 consecutive drops to lose
 * any entry.
 */
#define ACK_OVERLAP_COUNT 8

/**
 * Wire-acceptance cap for one ACK batch: fresh + overlap.
 */
#define MAX_ACK_ENTRIES_PER_BATCH (MAX_FRESH_ENTRIES_PER_BATCH + ACK_OVERLAP_COUNT)

#define ACK_ENTRY_SIZE 6

/**
 * Number of progressive passes emitted per Cdf53 tile.
 * = 1 sign-bit-plane + 13 magnitude bit-planes covering worst-case
 * 14-bit signed coefficients after 3 levels of CDF 5/3 lifting on
 * 8-bit input.
 */
#define CDF53_PASS_COUNT 14

/**
 * Per-tile coefficient count per channel: 1024 (32×32 tile).
 */
#define CDF53_COEFFS_PER_CHANNEL 1024

/**
 * Number of color channels encoded: BGR (alpha dropped, always 0xFF).
 */
#define CDF53_CHANNELS 3

/**
 * Total coefficients per tile = 3072.
 */
#define CDF53_TOTAL_COEFFS (CDF53_CHANNELS * CDF53_COEFFS_PER_CHANNEL)

/**
 * The most bytes one CDF 5/3 pass payload can take, whatever the pixels: three
 * channels of `[u16 len][RLE plane]`, an RLE plane at most twice its 128 bytes
 * (a literal `0x7F` or `0x80..=0xFF` costs a 2-byte escape), plus the 2-byte
 * `present_passes` prefix on pass 0. Reached exactly by an all-ones plane.
 */
#define MAX_PASS_PAYLOAD ((3 * (2 + (2 * (CDF53_COEFFS_PER_CHANNEL / 8)))) + 2)

/**
 * Maximum colors in a PalRLE palette. Tiles with more unique colors fall
 * through the classifier to Cdf53.
 */
#define MAX_PALETTE_COUNT 16

/**
 * Persistent palette table capacity.
 */
#define PALETTE_TABLE_SLOTS 256

/**
 * The most bytes one PalRle payload can take, whatever the tile: flags and
 * palette id (2), the bundled palette's count byte (1) and `MAX_PALETTE_COUNT`
 * BGRA colours, and the nibble-RLE of the 1024 indices at its worst, one byte
 * per pixel when no two neighbours match. (The `indices_raw` variant is 514
 * bytes, so the RLE form is the larger.) Reached exactly by a tile whose
 * indices alternate every pixel.
 */
#define MAX_PAL_RLE_PAYLOAD (((2 + 1) + (MAX_PALETTE_COUNT * 4)) + 1024)

/**
 * Sentinel tile coordinates marking an eviction notice. Distinct from
 * `FRAME_DIMENSIONS_SENTINEL_*` (0xFF) so the two control messages cannot
 * be confused for one another.
 */
#define EVICTION_SENTINEL_X 254

#define EVICTION_SENTINEL_Y 254

/**
 * Size of the parity packet header in bytes:
 *   - group_start: u16 BE (2 bytes)
 *   - group_len:   u8    (1 byte)
 */
#define PARITY_HEADER_SIZE 3

#define FEEDBACK_SIZE 18

/**
 * Receiver loss/suspension report (see [`crate::feedback::ReceiverFeedback`]).
 */
#define FEEDBACK_MSG_TYPE 1

/**
 * One-shot capability advertisement, `[HELLO_MSG_TYPE, caps]`.
 */
#define HELLO_MSG_TYPE 3

/**
 * Encoded length of a HELLO message.
 */
#define HELLO_SIZE 2

/**
 * Per-tile decode failure report.
 */
#define DECODE_ERROR_MSG_TYPE 4

/**
 * Input event (pointer, wheel, key). A sub-kind byte follows.
 */
#define INPUT_MSG_TYPE 5

/**
 * Client display capability advertisement.
 */
#define DISPLAY_INFO_MSG_TYPE 7

/**
 * Client display mode request.
 */
#define DISPLAY_MODE_MSG_TYPE 8

/**
 * HELLO capability bit 0: the client can decode `indices_raw` PalRLE
 * payloads (PalRLE flags bit 1).
 */
#define HELLO_CAP_INDICES_RAW 1

/**
 * HELLO capability bit 1: the client can decode H.264 and sample the
 * resulting `GPUExternalTexture` (`texture_external`). Firefox WebGPU
 * cannot, and sends this bit clear so the server never emits unplayable
 * H.264.
 */
#define HELLO_CAP_SUPPORTS_H264 2

/**
 * HELLO capability bits that must be zero today; the server ignores them.
 */
#define HELLO_CAPS_RESERVED_MASK 252

#define DATAGRAM_HEADER_SIZE 16

#define TILE_HEADER_SIZE 8

/**
 * Sentinel `wire_seq` value meaning "not yet stamped by the emitter".
 * Used at construction sites (`fragment_tile`, `build_parity_datagrams`)
 * to make the convention named and greppable. The `ReliableTileEmitter`
 * overwrites this with a real per-session monotonic value at submit time.
 */
#define UNSTAMPED_WIRE_SEQ 0

/**
 * Registry of sentinel tile coordinates allocated for control messages.
 * Tile coords are `u8`; each allocation below is structurally impossible
 * as a real tile at any sensible resolution, so the receiver routes on it
 * without a new datagram type or version negotiation. Allocated so far:
 *
 * - `(0xFF, 0xFF)` -- `FRAME_DIMENSIONS_SENTINEL_X`/`_Y`, below.
 * - `(0xFE, 0xFE)` -- `eviction::EVICTION_SENTINEL_X`/`_Y`
 *   (`ghostframe-protocol/src/eviction.rs`).
 *
 * Each sentinel consumed here is a real cost, not a free bit: it lowers
 * the maximum addressable tile column/row by one. Two allocations put the
 * ceiling at 254 tiles (8128 px at the 32px tile size this protocol uses),
 * down from 255 (8160 px) with one. Both are far beyond any resolution
 * this system targets, but the trade is real, so it is recorded here
 * rather than discovered by a future implementer doing the arithmetic.
 *
 * Allocate a fourth control message by extending this list -- or by
 * introducing a `ControlKind` predicate that decodes intent from the
 * sentinel pair instead of adding more magic coordinates, if the list
 * grows past a couple more entries.
 *
 * Sentinel tile coordinates marking a control message that carries the
 * current frame dimensions rather than pixel data. Tile coords are `u8`;
 * 0xFF (255) is structurally impossible at any sensible resolution
 * (would imply >8000 px width), so the receiver can route on the sentinel.
 */
#define FRAME_DIMENSIONS_SENTINEL_X 255

#define FRAME_DIMENSIONS_SENTINEL_Y 255

/**
 * Bit 31 of frame_seq distinguishes tile datagrams from frame datagrams.
 * Frame datagrams: bit 31 = 0. Tile datagrams: bit 31 = 1.
 */
#define TILE_DATAGRAM_FLAG (1 << 31)

#define FRAME_HEADER_SIZE 14

/**
 * Envelope discriminator byte for the tile-FEC parity datagram.
 */
#define TILE_PARITY_ENVELOPE 4

/**
 * Size of the fixed-length part of the header, preceding the `source_lens`
 * table and `parity_payload`.
 */
#define TILE_PARITY_HEADER_SIZE (((1 + 4) + 1) + 1)

/**
 * Envelope discriminator byte for the per-pass NACK datagram
 * (client → server).
 */
#define TILE_NACK_ENVELOPE 5

/**
 * Size on the wire of a single NACK entry.
 */
#define TILE_NACK_ENTRY_SIZE 7

/**
 * Maximum number of NACK entries per datagram. Mirrors the ACK envelope's
 * cap so a NackBatcher can use the same chunking pattern.
 */
#define TILE_NACK_MAX_ENTRIES 64

#define TILE_SIZE 32

#define BPP 4

#define TILE_BYTES (uintptr_t)((TILE_SIZE * TILE_SIZE) * BPP)

/**
 * Bundles a `GhostframeServer` with the tokio `Runtime` that owns its
 * background tasks.  Drop order matters: the server (and its spawned
 * IoBridge task) must be dropped *before* the runtime shuts down.
 *
 * Opaque to C callers (cbindgen emits a forward declaration).
 * Rust callers should use `GhostframeServer` directly.
 */
typedef struct FfiHandle FfiHandle;

/**
 * Opaque pointer returned to C callers.
 */
typedef struct FfiHandle *GfServerHandle;

/**
 * Create and start a new GhostframeServer.
 * Returns a handle on success, or null on failure.
 *
 * # Safety
 * All string pointers must be valid, NUL-terminated C strings.
 */
GfServerHandle gf_server_new(const char *hostname,
                             const char *authkey,
                             const char *state_dir,
                             const char *control_url);

/**
 * Submit a frame for tiling and transmission.
 * Returns 0 on success, -1 on failure (including a null argument, or a frame
 * geometry that is zero, larger than 16384 per edge, has a stride below
 * `width * 4`, or spans 4 GiB or more).
 *
 * # Safety
 * `handle` must be a valid pointer from `gf_server_new`.
 * `pixels` must be valid for `stride * height` bytes.
 */
int32_t gf_server_submit_frame(GfServerHandle handle,
                               uint32_t width,
                               uint32_t height,
                               uint32_t stride,
                               const uint8_t *pixels,
                               uint32_t timestamp_us);

/**
 * Number of WebTransport clients currently connected.
 *
 * `0` means no consumer exists for a frame, so the caller's capture loop
 * should skip the scrape entirely: without this gate a capture backend keeps
 * copying full framebuffers at the frame rate regardless of demand. Returns
 * `0` for a null handle, which is also the "do not capture" answer — a
 * caller that lost its handle has nothing to feed anyway.
 *
 * Non-blocking. Prefer `gf_server_wait_for_client` while idle rather than
 * polling this in a sleep loop: the sleep interval lands in full between a
 * client connecting and the first frame being captured.
 *
 * # Safety
 * `handle` must be a valid pointer from `gf_server_new`, or null.
 */
uintptr_t gf_server_connected_session_count(GfServerHandle handle);

/**
 * Block until at least one WebTransport client is connected.
 *
 * This is the idle gate for a C capture loop: it wakes on the connect event
 * itself, so the first frame goes out without the latency a poll interval
 * would add. Returns as soon as a client is already connected.
 *
 * `timeout_ms` of 0 waits indefinitely.
 *
 * Returns:
 * - `> 0` — the number of connected clients; start capturing.
 * - `0`   — `timeout_ms` elapsed with no client. Call again to keep waiting.
 * - `-1`  — terminal: null handle, or the server event loop has exited and no
 *   client can ever arrive. The caller should stop, not retry.
 *
 * Blocks the calling thread. The server's own event loop runs on its internal
 * multi-threaded runtime and keeps making progress meanwhile, which is what
 * lets the client this call is waiting for actually connect.
 *
 * # Safety
 * `handle` must be a valid pointer from `gf_server_new`, or null.
 */
int32_t gf_server_wait_for_client(GfServerHandle handle, uint32_t timeout_ms);

/**
 * Destroy a GhostframeServer and free its resources.
 *
 * # Safety
 * `handle` must be a valid pointer from `gf_server_new`, or null.
 */
void gf_server_destroy(GfServerHandle handle);
