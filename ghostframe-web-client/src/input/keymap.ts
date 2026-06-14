// X11 KeySym translation for browser KeyboardEvent.key values.
//
// Two paths:
//   1. Named keys (Enter, ArrowUp, F-keys, modifiers, ...) - lookup table.
//   2. Printable single-codepoint text - X11 keysym-from-Unicode rule:
//      U+0020..U+00FF maps directly; higher BMP ORs with 0x01000000.
//
// Spec: docs/superpowers/specs/2026-06-13-input-forwarding-design.md

// Selected from /usr/include/X11/keysymdef.h. Names match
// KeyboardEvent.key, NOT KeyboardEvent.code.
const NAMED_KEYSYMS: Record<string, number> = {
  // Whitespace / control
  Backspace: 0xff08,
  Tab: 0xff09,
  Enter: 0xff0d,
  Escape: 0xff1b,
  Delete: 0xffff,

  // Navigation
  Home: 0xff50,
  End: 0xff57,
  PageUp: 0xff55,
  PageDown: 0xff56,
  ArrowLeft: 0xff51,
  ArrowUp: 0xff52,
  ArrowRight: 0xff53,
  ArrowDown: 0xff54,

  // Editing
  Insert: 0xff63,

  // Function keys
  F1: 0xffbe,
  F2: 0xffbf,
  F3: 0xffc0,
  F4: 0xffc1,
  F5: 0xffc2,
  F6: 0xffc3,
  F7: 0xffc4,
  F8: 0xffc5,
  F9: 0xffc6,
  F10: 0xffc7,
  F11: 0xffc8,
  F12: 0xffc9,

  // Modifiers - map to the *_L variants. KeyboardEvent.key reports
  // "Shift" for both shifts; the server doesn't usually care, but if
  // it does we'd need to look at event.location.
  Shift: 0xffe1,
  Control: 0xffe3,
  Alt: 0xffe9,
  Meta: 0xffeb,
  AltGraph: 0xfe03,
  CapsLock: 0xffe5,
  NumLock: 0xff7f,
  ScrollLock: 0xff14,

  // Misc
  PrintScreen: 0xff61,
  Pause: 0xff13,
  ContextMenu: 0xff67,
  Space: 0x0020, // KeyboardEvent.key for space is " ", but be defensive
};

/** Return the X11 KeySym for a DOM KeyboardEvent, or null if it should
 * not be sent (e.g. dead-key composing state). */
export function keyboardEventToKeysym(event: KeyboardEvent): number | null {
  const k = event.key;

  // Dead keys: the browser will fire a follow-up event with the resolved
  // character. Don't send anything yet.
  if (k === 'Dead' || k === 'Unidentified') return null;

  // Named keys take priority.
  if (k in NAMED_KEYSYMS) return NAMED_KEYSYMS[k];

  // Single character: Unicode → X11 keysym.
  if (k.length === 1) {
    const cp = k.codePointAt(0)!;
    if (cp >= 0x20 && cp <= 0xff) return cp;
    return cp | 0x01000000;
  }

  // Unknown multi-char name we don't have in the table.
  return null;
}
