/**
 * Create a `GPUShaderModule` with a debug label and surface any WGSL
 * compilation messages to the browser console.
 *
 * Without an explicit label the browser's "shader is invalid" diagnostic
 * just says `ShaderModule with '' label is invalid` — useless when 10+
 * shaders are created at startup. Without proactively awaiting
 * `getCompilationInfo()` the underlying WGSL error (line number,
 * message, e.g. "unknown type 'texture_external'") never reaches the
 * console at all in Firefox.
 *
 * The promise is intentionally fire-and-forget: callers stay synchronous
 * (pipeline construction sequence is unchanged) but any failures land
 * in the console with the labelled module name as `[shader:<label>] …`.
 */
export function createLabeledShaderModule(
  device: GPUDevice,
  label: string,
  code: string,
): GPUShaderModule {
  const module = device.createShaderModule({ label, code });
  module.getCompilationInfo().then(
    (info) => {
      for (const msg of info.messages) {
        const where = `L${msg.lineNum}:C${msg.linePos}`;
        const prefix = `[shader:${label}] ${where} ${msg.type}:`;
        if (msg.type === 'error') {
          console.error(prefix, msg.message);
        } else if (msg.type === 'warning') {
          console.warn(prefix, msg.message);
        } else {
          console.info(prefix, msg.message);
        }
      }
    },
    (err) => {
      console.error(`[shader:${label}] getCompilationInfo failed:`, err);
    },
  );
  return module;
}
