export interface WebGpuInitResult {
  adapter: GPUAdapter;
  device: GPUDevice;
  context: GPUCanvasContext;
  presentFormat: GPUTextureFormat;
}

export class WebGpuUnavailableError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'WebGpuUnavailableError';
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * Acquire a WebGPU device and configure the canvas context. Throws
 * `WebGpuUnavailableError` if WebGPU is not available — the caller is
 * expected to surface this as a fatal UI error per design D2.
 *
 * On first launch Chrome's GPU process initialises asynchronously; Dawn may
 * briefly return null from requestAdapter() before the Vulkan backend is ready.
 * We retry up to 5 times with 500 ms delays (≤2.5 s total) before giving up.
 */
export async function initWebGpu(canvas: HTMLCanvasElement): Promise<WebGpuInitResult> {
  if (!('gpu' in navigator) || !navigator.gpu) {
    throw new WebGpuUnavailableError('navigator.gpu is undefined — WebGPU not available');
  }
  let adapter: GPUAdapter | null = null;
  // Retry up to 10 × 1 s = 10 s total.  Chrome's GPU process initialises
  // asynchronously; Dawn sometimes reports null on the very first call before
  // the Vulkan backend is ready.
  const MAX_RETRIES = 10;
  for (let attempt = 0; attempt <= MAX_RETRIES; attempt++) {
    adapter = await navigator.gpu.requestAdapter();
    if (adapter) break;
    if (attempt < MAX_RETRIES) {
      await sleep(1000);
    }
  }
  if (!adapter) {
    throw new WebGpuUnavailableError('navigator.gpu.requestAdapter() returned null');
  }
  const device = await adapter.requestDevice();
  const context = canvas.getContext('webgpu');
  if (!context) {
    throw new WebGpuUnavailableError('canvas.getContext("webgpu") returned null');
  }
  // rgba8unorm is the M3.2b chosen presentation format (Design D12).
  const presentFormat: GPUTextureFormat = 'rgba8unorm';
  context.configure({
    device,
    format: presentFormat,
    alphaMode: 'opaque',
  });
  return { adapter, device, context, presentFormat };
}
