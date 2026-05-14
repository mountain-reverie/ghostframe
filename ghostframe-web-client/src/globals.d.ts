/// <reference types="@webgpu/types" />

declare module '*.wgsl?raw' {
  const content: string;
  export default content;
}

// Type augmentation for ad-hoc test-instrumentation properties on `window`.
// Polled by Playwright in `e2e_mode_switch` (see `ghostframe-lib/tests/e2e.rs`).

interface Window {
  __ghostframeStats: { tileDatagrams: number; frameDatagrams: number };
}
