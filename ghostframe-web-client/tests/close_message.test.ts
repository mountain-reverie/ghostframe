import { describe, it, expect } from 'vitest';
import { closeOverlayMessage, CLOSE_CODE_ACCESS_REVOKED } from '../src/close_message';

describe('closeOverlayMessage', () => {
  it('says "Access revoked" for the server code 0x6704', () => {
    expect(CLOSE_CODE_ACCESS_REVOKED).toBe(26372);
    expect(closeOverlayMessage({ closeCode: 0x6704 })).toBe('Access revoked');
  });

  it('stays silent for other closes', () => {
    expect(closeOverlayMessage({ closeCode: 0x6701 })).toBeNull();
    expect(closeOverlayMessage({ closeCode: 0 })).toBeNull();
    expect(closeOverlayMessage({})).toBeNull();
    expect(closeOverlayMessage(null)).toBeNull();
  });
});
