import { describe, expect, it } from 'vitest';
import { Health, installConsoleRing, health } from '../src/health';

describe('Health', () => {
  it('counts swallowed errors per site and keeps the first message', () => {
    const h = new Health();
    h.record('handleEvent:TilePayload', new Error('boom'));
    h.record('handleEvent:TilePayload', new Error('again'));
    h.record('encodeAndPresentFrame', 'str');
    const s = h.snapshot();
    expect(s.swallowed).toBe(3);
    expect(s.bySite['handleEvent:TilePayload']).toBe(2);
    expect(s.first).toBe('handleEvent:TilePayload: boom');
    expect(s.recent).toHaveLength(3);
  });

  it('bounds the recent list and the console ring', () => {
    const h = new Health();
    for (let i = 0; i < 50; i++) h.record('x', i);
    for (let i = 0; i < 1000; i++) h.note(`l${i}`);
    const s = h.snapshot();
    expect(s.swallowed).toBe(50);
    expect(s.recent.length).toBeLessThanOrEqual(20);
    expect(s.console.length).toBeLessThanOrEqual(300);
    expect(s.console[s.console.length - 1]).toBe('l999');
  });

  it('mirrors console.error and console.warn into the ring', () => {
    const fake = { warn: () => {}, error: () => {} } as unknown as Console;
    installConsoleRing(fake);
    fake.error('bad', new Error('e'));
    fake.warn('careful');
    const lines = health.snapshot().console;
    expect(lines.some((l) => l.startsWith('error: bad e'))).toBe(true);
    expect(lines.some((l) => l === 'warn: careful')).toBe(true);
  });
});
