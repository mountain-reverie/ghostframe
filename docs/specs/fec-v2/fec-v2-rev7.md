# FEC v2, revision 7: the frame header (header diet, wire freeze)

Revision 7 replaces the frame header layout of [rev 6 §6.1](fec-v2-rev6.md)
and nothing else. Everything outside the three header layouts below (the
bundle and block model, the coded symbol, the repair coefficients, the
feedback streams, bounds) is rev 6, unchanged.

Every server-to-client datagram is one frame. Integers are little-endian.

```
byte 0: kind (bits 7-6) | 6 kind-specific bits

kind 00  reserved: rejected. The escape for any later format.
kind 01  SOURCE   bit 5 LAST | bits 4-2 T_adv (span code 0..=7) | bits 1-0 scheme
kind 10  REPAIR   bits 5-2 RTT hint code (0..=15)               | bits 1-0 scheme
kind 11  PLAIN    bits 5-0 reserved, must be 0 (rejected otherwise)

SOURCE  8 B  byte0, bundle_seq u32, block_id u16, idx u8, records...
REPAIR  9 B  byte0, bundle_seq u32, block_id u16, k u8, repair_idx u8, payload...
PLAIN   5 B  byte0, bundle_seq u32, records...
```

- Scheme 0 is RS Cauchy over GF(2^8). Any other scheme is `UnknownScheme`.
- REPAIR is valid when `k >= 1` and `k + repair_idx <= 255`, which is the
  codec's row bound (`k + row + 1 <= 256`). The planned repair count `r` of
  rev 6 is no longer on the wire; no receiver used it beyond that check.
- Kind 00 is `UnknownKind`. The rev 6 kind bytes 0x01, 0x02 and 0x03 all fall
  in kind 00, so a rev 6 peer's frames are rejected outright, never misread.
  PLAIN with a reserved bit set is `PlainReservedBits`.

## Savings

| frame  | rev 6 | rev 7 | saved |
|--------|-------|-------|-------|
| SOURCE | 9 B   | 8 B   | 1 B   |
| REPAIR | 11 B  | 9 B   | 2 B   |
| PLAIN  | 5 B   | 5 B   | 0     |

`FEC_RECORDS_HEADROOM` (the REPAIR header plus the 6-byte coded-symbol
overhead) goes from 17 to 15, so a bundle carries 2 more record bytes.

## Frozen

This header is frozen. Any further change needs a new sub-format selected
under kind 00, which rev 7 receivers reject today; it must not reuse or
reinterpret a bit of kinds 01 to 11.

Implementation: `ghostframe-protocol/src/fec/frame.rs`, whose golden tests
pin the exact bytes.
