/** QUIC application close code the server uses for a session whose peer no
 * longer passes the tailnet access rule (ghostframe-lib, `CLOSE_CODE_ACCESS_REVOKED`). */
export const CLOSE_CODE_ACCESS_REVOKED = 0x6704;

/**
 * What to tell the user when the transport closes with `info`, or null when
 * the close needs no message beyond the console log (the existing Evicted
 * overlay covers takeover, and other closes are not explained to the user).
 */
export function closeOverlayMessage(
  info: { closeCode?: number } | null | undefined,
): string | null {
  return info?.closeCode === CLOSE_CODE_ACCESS_REVOKED ? 'Access revoked' : null;
}
