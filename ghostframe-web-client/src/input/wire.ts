// Browser-side input capture wiring.
//
// `attachInputCapture(canvas, feedbackWriter, getFrameDims)`:
//   - sets tabIndex on the canvas so it can hold keyboard focus
//   - hooks pointer / wheel / keyboard events
//   - coalesces pointermove to 30 Hz via requestAnimationFrame
//   - scales canvas-pixel coords → framebuffer-pixel coords using
//     getFrameDims() (provided by main.ts, sourced from the dimensions
//     datagram)
//   - sends each event over the existing feedback bidi stream
//
// Spec: docs/superpowers/specs/2026-06-13-input-forwarding-design.md

import {
  encodePointerMove,
  encodePointerButton,
  encodeWheel,
  encodeKeyDown,
  encodeKeyUp,
} from './encode.js';
import { keyboardEventToKeysym } from './keymap.js';

/** Caller-provided lookup for the current framebuffer dimensions. The
 * web-client framebuffer module exposes `framebuffer.width/height`. */
export type FrameDimsGetter = () => { width: number; height: number };

/** Browser DOM button index → X11 button number.
 *   DOM 0=left,1=middle,2=right,3=back,4=forward
 *   X11 1=left,2=middle,3=right,8=back,9=forward */
function domButtonToX11(b: number): number | null {
  switch (b) {
    case 0:
      return 1;
    case 1:
      return 2;
    case 2:
      return 3;
    case 3:
      return 8;
    case 4:
      return 9;
    default:
      return null;
  }
}

/** Round-half-toward-zero for fb-pixel quantization. */
function quantize(n: number): number {
  return Math.max(-32768, Math.min(32767, Math.trunc(n)));
}

export function attachInputCapture(
  canvas: HTMLCanvasElement,
  feedbackWriter: WritableStreamDefaultWriter<Uint8Array>,
  getFrameDims: FrameDimsGetter,
): void {
  // Allow keyboard focus.
  canvas.tabIndex = 0;

  // Pending coalesced pointermove (in FB coords).
  let pending: { x: number; y: number } | null = null;
  let rafQueued = false;

  function flushPending(): void {
    rafQueued = false;
    if (!pending) return;
    const { x, y } = pending;
    pending = null;
    void send(encodePointerMove(x, y));
  }

  function schedulePointerMove(x: number, y: number): void {
    pending = { x, y };
    if (!rafQueued) {
      rafQueued = true;
      requestAnimationFrame(flushPending);
    }
  }

  async function send(bytes: Uint8Array): Promise<void> {
    try {
      await feedbackWriter.write(bytes);
    } catch (e) {
      // Feedback stream closed mid-session — same posture as main.ts's
      // HELLO write failure. Don't spam: log once per writer.
      console.warn('input send failed:', e);
    }
  }

  function canvasToFb(clientX: number, clientY: number): { x: number; y: number } {
    const rect = canvas.getBoundingClientRect();
    const cx = clientX - rect.left;
    const cy = clientY - rect.top;
    const cw = rect.width;
    const ch = rect.height;
    const dims = getFrameDims();
    if (cw <= 0 || ch <= 0 || dims.width <= 0 || dims.height <= 0) {
      return { x: 0, y: 0 };
    }
    return {
      x: quantize((cx / cw) * dims.width),
      y: quantize((cy / ch) * dims.height),
    };
  }

  canvas.addEventListener('pointermove', (e) => {
    const { x, y } = canvasToFb(e.clientX, e.clientY);
    schedulePointerMove(x, y);
  });

  canvas.addEventListener('pointerdown', (e) => {
    canvas.focus();
    const btn = domButtonToX11(e.button);
    if (btn === null) return;
    const { x, y } = canvasToFb(e.clientX, e.clientY);
    void send(encodePointerButton(x, y, btn, true));
    e.preventDefault();
  });

  canvas.addEventListener('pointerup', (e) => {
    const btn = domButtonToX11(e.button);
    if (btn === null) return;
    const { x, y } = canvasToFb(e.clientX, e.clientY);
    void send(encodePointerButton(x, y, btn, false));
    e.preventDefault();
  });

  canvas.addEventListener('pointerleave', () => {
    // Marker so the server can hide its cursor while the user is in the
    // browser UI.
    void send(encodePointerMove(-1, -1));
  });

  canvas.addEventListener(
    'wheel',
    (e) => {
      // Per-tick deltas: clamp to ±N and send. Browsers report deltaY in
      // various units depending on deltaMode; we treat any non-zero as
      // one wheel tick for v1.
      const dx = e.deltaX > 0 ? 1 : e.deltaX < 0 ? -1 : 0;
      const dy = e.deltaY > 0 ? 1 : e.deltaY < 0 ? -1 : 0;
      if (dx || dy) {
        void send(encodeWheel(dx, dy));
      }
      e.preventDefault();
    },
    { passive: false },
  );

  canvas.addEventListener('keydown', (e) => {
    const sym = keyboardEventToKeysym(e);
    if (sym !== null) {
      void send(encodeKeyDown(sym));
      e.preventDefault();
    }
  });

  canvas.addEventListener('keyup', (e) => {
    const sym = keyboardEventToKeysym(e);
    if (sym !== null) {
      void send(encodeKeyUp(sym));
      e.preventDefault();
    }
  });
}
