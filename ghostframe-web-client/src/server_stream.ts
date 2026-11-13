// SPDX-License-Identifier: MIT OR Apache-2.0
/**
 * Reads the feedback stream's return half and hands each chunk to `onChunk`.
 *
 * The server writes the stats schema and samples there (see
 * `ghostframe-protocol/src/stats.rs`). Chunks are arbitrary slices of that
 * byte stream; the core reassembles frames across them, so this only moves
 * bytes. One chunk that makes `onChunk` throw is logged and skipped: it must
 * not end the reader, since the next chunk may be fine.
 *
 * Resolves when the stream ends or errors (a closing session ends it).
 */
export async function pumpServerStream(
  readable: ReadableStream<Uint8Array>,
  onChunk: (bytes: Uint8Array) => void,
  warn: (msg: string, err: unknown) => void = (m, e) => console.warn(m, e),
): Promise<void> {
  const reader = readable.getReader();
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) return;
      if (!value || value.length === 0) continue;
      try {
        onChunk(value);
      } catch (e) {
        warn('server stream: chunk handler failed; continuing', e);
      }
    }
  } catch (e) {
    warn('server stream ended with an error', e);
  } finally {
    reader.releaseLock();
  }
}
