#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

/**
 * M3.3c escalation cap: maximum number of idle-escalation candidates the
 * io_bridge will dispatch in a single frame. Bounds GPU memory for the
 * dedicated escalation_coefficients_buffer (K × 3072 × 4 = 6 MB at K=512).
 */
#define MAX_ESCALATION_PER_FRAME 512

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
 * Maximum colors in a PalRLE palette. Tiles with more unique colors fall
 * through the classifier to BC1/Cdf53.
 */
#define MAX_PALETTE_COUNT 16

/**
 * Persistent palette table capacity.
 */
#define PALETTE_TABLE_SLOTS 256

#define TILE_SIZE 32

#define BPP 4

#define TILE_BYTES (uintptr_t)((TILE_SIZE * TILE_SIZE) * BPP)

/**
 * Sentinel value for `TileMetrics::unique_colors` indicating the GPU compute
 * estimator has not run yet (M3.0 always uses this — backing lands in M3.3).
 * Classifier rules consulting `unique_colors` treat the sentinel as "unknown".
 */
#define UNIQUE_COLORS_UNKNOWN UINT16_MAX



/**
 * Threshold matching the spec: tile must be idle for > IDLE_THRESHOLD frames
 * before becoming eligible for escalation. 30 frames ≈ 500 ms at 60 fps.
 */
#define IDLE_THRESHOLD 30

#define ACK_BATCH_MSG_TYPE 3

#define MAX_ACK_ENTRIES_PER_BATCH 64

#define ACK_ENTRY_SIZE 7

#define HELLO_MSG_TYPE 3

#define HELLO_SIZE 2

#define DECODE_ERROR_MSG_TYPE 4

#define DECODE_ERROR_SIZE 5

#define ERR_PAYLOAD_TOO_SHORT 1

#define ERR_COUNT_OUT_OF_RANGE 2

#define ERR_THIN_UNCACHED_PALETTE 3

#define ERR_BUNDLED_TRUNCATED 4

#define ERR_INDEX_OOB 5

#define ERR_RLE_OVERSHOOT 6

#define ERR_RLE_UNDERSHOOT 7

/**
 * Size of the parity packet header in bytes:
 *   - group_start: u16 BE (2 bytes)
 *   - group_len:   u8    (1 byte)
 */
#define PARITY_HEADER_SIZE 3

#define FEEDBACK_MSG_TYPE 1

#define FEEDBACK_SIZE 22

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

#define DATAGRAM_HEADER_SIZE 12

#define TILE_HEADER_SIZE 8

/**
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
 * NACK message size: frame_seq (4) + frag_idx (2) = 6 bytes.
 */
#define NACK_SIZE 6

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
 * Returns 0 on success, -1 on failure.
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
 * Destroy a GhostframeServer and free its resources.
 *
 * # Safety
 * `handle` must be a valid pointer from `gf_server_new`, or null.
 */
void gf_server_destroy(GfServerHandle handle);

extern int gbridge_new(const char *hostname,
                       const char *authkey,
                       const char *state_dir,
                       const char *control_url,
                       int *sd_out);

extern int gbridge_up(int sd);

extern int gbridge_listen_udp(int sd, const char *addr, int *fd_out);

extern int gbridge_dial_udp(int sd, const char *remote_addr, int *fd_out);

extern int gbridge_close(int sd);

extern int gbridge_getips(int sd, char *buf, uintptr_t buf_len);
