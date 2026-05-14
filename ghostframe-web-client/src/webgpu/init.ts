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

/**
 * Acquire a WebGPU device and configure the canvas context. Throws
 * `WebGpuUnavailableError` if WebGPU is not available — the caller is
 * expected to surface this as a fatal UI error per design D2.
 */
export async function initWebGpu(canvas: HTMLCanvasElement): Promise<WebGpuInitResult> {
  if (!('gpu' in navigator) || !navigator.gpu) {
    throw new WebGpuUnavailableError('navigator.gpu is undefined — WebGPU not available');
  }
  const adapter = await navigator.gpu.requestAdapter();
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
