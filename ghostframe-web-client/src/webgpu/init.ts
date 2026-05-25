export interface WebGpuInitResult {
  adapter: GPUAdapter;
  device: GPUDevice;
  context: GPUCanvasContext;
  presentFormat: GPUTextureFormat;
  /** True when running on SwiftShader (software renderer). */
  isSwiftShader: boolean;
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

  // Detect SwiftShader (software renderer) via adapter info. On SwiftShader,
  // WebCodecs VideoDecoder crashes the GPU process; callers should skip H.264
  // decode when isSwiftShader=true.
  let isSwiftShader = false;
  try {
    // adapter.info is synchronous per the WebGPU spec (Chrome 121+).
    const info = adapter.info;
    const desc = (info.description ?? '').toLowerCase();
    const vendor = (info.vendor ?? '').toLowerCase();
    const arch = ((info as any).architecture ?? '').toLowerCase();
    isSwiftShader = desc.includes('swiftshader')
      || arch.includes('swiftshader')
      || (vendor === '' && desc === ''); // fallback: empty adapter info usually means SwiftShader in headless
    (window as any).__gpuAdapterInfo = { vendor, description: desc, architecture: arch };
  } catch {
    // adapter.info not available — also try isFallbackAdapter
    try {
      isSwiftShader = !!(adapter as any).isFallbackAdapter;
    } catch { /* ignore */ }
  }

  return { adapter, device, context, presentFormat, isSwiftShader };
}
