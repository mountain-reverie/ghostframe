import { describe, expect, test } from 'vitest';
import { shouldSkipEncodePresent } from '../src/webgpu/idle_skip';

describe('shouldSkipEncodePresent', () => {
  test('skips when every queue is empty and framebuffer not dirty', () => {
    expect(
      shouldSkipEncodePresent({
        rawQueueLen: 0,
        solidQueueLen: 0,
        palRleQueueLen: 0,
        cdf53QueueLen: 0,
        h264QueueLen: 0,
        framebufferDirty: false,
        errorsReadbackInFlight: false,
      }),
    ).toBe(true);
  });

  test('does NOT skip when any queue is non-empty', () => {
    for (const k of [
      'rawQueueLen',
      'solidQueueLen',
      'palRleQueueLen',
      'cdf53QueueLen',
      'h264QueueLen',
    ] as const) {
      const base = {
        rawQueueLen: 0,
        solidQueueLen: 0,
        palRleQueueLen: 0,
        cdf53QueueLen: 0,
        h264QueueLen: 0,
        framebufferDirty: false,
        errorsReadbackInFlight: false,
      };
      base[k] = 1;
      expect(shouldSkipEncodePresent(base)).toBe(false);
    }
  });

  test('does NOT skip when framebuffer is dirty even if queues empty', () => {
    expect(
      shouldSkipEncodePresent({
        rawQueueLen: 0,
        solidQueueLen: 0,
        palRleQueueLen: 0,
        cdf53QueueLen: 0,
        h264QueueLen: 0,
        framebufferDirty: true,
        errorsReadbackInFlight: false,
      }),
    ).toBe(false);
  });

  test('does NOT skip while a palrle errors readback is in flight', () => {
    expect(
      shouldSkipEncodePresent({
        rawQueueLen: 0,
        solidQueueLen: 0,
        palRleQueueLen: 0,
        cdf53QueueLen: 0,
        h264QueueLen: 0,
        framebufferDirty: false,
        errorsReadbackInFlight: true,
      }),
    ).toBe(false);
  });
});
