// SPDX-License-Identifier: MIT OR Apache-2.0
import { describe, it, expect } from 'vitest';
import { pumpServerStream } from '../src/server_stream';

function streamOf(chunks: Uint8Array[], failWith?: unknown): ReadableStream<Uint8Array> {
  let i = 0;
  return new ReadableStream<Uint8Array>({
    pull(controller) {
      if (i < chunks.length) controller.enqueue(chunks[i++]);
      else if (failWith !== undefined) controller.error(failWith);
      else controller.close();
    },
  });
}

describe('pumpServerStream', () => {
  it('delivers every non-empty chunk in order and resolves at the end', async () => {
    const seen: number[][] = [];
    await pumpServerStream(
      streamOf([new Uint8Array([1, 2]), new Uint8Array([]), new Uint8Array([3])]),
      (b) => seen.push(Array.from(b)),
    );
    expect(seen).toEqual([[1, 2], [3]]);
  });

  it('keeps reading after a chunk handler throws', async () => {
    const seen: number[] = [];
    const warnings: string[] = [];
    await pumpServerStream(
      streamOf([new Uint8Array([1]), new Uint8Array([2]), new Uint8Array([3])]),
      (b) => {
        if (b[0] === 2) throw new Error('boom');
        seen.push(b[0]);
      },
      (m) => warnings.push(m),
    );
    expect(seen).toEqual([1, 3]);
    expect(warnings).toHaveLength(1);
  });

  it('resolves, and warns, when the stream errors', async () => {
    const warnings: string[] = [];
    await pumpServerStream(streamOf([new Uint8Array([1])], new Error('reset')), () => {}, (m) =>
      warnings.push(m),
    );
    expect(warnings).toHaveLength(1);
  });
});
