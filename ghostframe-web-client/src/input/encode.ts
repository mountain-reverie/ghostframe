// Wire encoders for INPUT_MSG_TYPE (0x05).
//
// Layout (big-endian, matching FEEDBACK/HELLO/DECODE_ERROR):
//   pointer-move    [0x05][0x01][x:i16][y:i16]                    6 bytes
//   pointer-button  [0x05][0x02][x:i16][y:i16][button:u8][down:u8] 8 bytes
//   wheel           [0x05][0x03][dx:i16][dy:i16]                  6 bytes
//   key-down        [0x05][0x04][keysym:u32]                      6 bytes
//   key-up          [0x05][0x05][keysym:u32]                      6 bytes
//
// Spec: docs/superpowers/specs/2026-06-13-input-forwarding-design.md

export const INPUT_MSG_TYPE = 0x05;

const SUB_POINTER_MOVE = 0x01;
const SUB_POINTER_BUTTON = 0x02;
const SUB_WHEEL = 0x03;
const SUB_KEY_DOWN = 0x04;
const SUB_KEY_UP = 0x05;

export function encodePointerMove(x: number, y: number): Uint8Array {
  const buf = new Uint8Array(6);
  const view = new DataView(buf.buffer);
  buf[0] = INPUT_MSG_TYPE;
  buf[1] = SUB_POINTER_MOVE;
  view.setInt16(2, x, false);
  view.setInt16(4, y, false);
  return buf;
}

export function encodePointerButton(
  x: number,
  y: number,
  button: number,
  down: boolean,
): Uint8Array {
  const buf = new Uint8Array(8);
  const view = new DataView(buf.buffer);
  buf[0] = INPUT_MSG_TYPE;
  buf[1] = SUB_POINTER_BUTTON;
  view.setInt16(2, x, false);
  view.setInt16(4, y, false);
  buf[6] = button & 0xff;
  buf[7] = down ? 1 : 0;
  return buf;
}

export function encodeWheel(dx: number, dy: number): Uint8Array {
  const buf = new Uint8Array(6);
  const view = new DataView(buf.buffer);
  buf[0] = INPUT_MSG_TYPE;
  buf[1] = SUB_WHEEL;
  view.setInt16(2, dx, false);
  view.setInt16(4, dy, false);
  return buf;
}

export function encodeKeyDown(keysym: number): Uint8Array {
  return encodeKey(SUB_KEY_DOWN, keysym);
}

export function encodeKeyUp(keysym: number): Uint8Array {
  return encodeKey(SUB_KEY_UP, keysym);
}

function encodeKey(sub: number, keysym: number): Uint8Array {
  const buf = new Uint8Array(6);
  const view = new DataView(buf.buffer);
  buf[0] = INPUT_MSG_TYPE;
  buf[1] = sub;
  view.setUint32(2, keysym >>> 0, false);
  return buf;
}
