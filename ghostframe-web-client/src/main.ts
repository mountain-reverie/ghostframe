// SPDX-License-Identifier: MIT OR Apache-2.0
// Page entry: starts the Rust client (ghostframe-client-wasm/src/app) with the
// e2e bookkeeping hooks (hooks.ts). PR 4 moves the hooks into Rust; PR 5
// deletes this file with the npm build.
import init, { startClient } from '../pkg-web/ghostframe_client_wasm.js';
import * as wasmExports from '../pkg-web/ghostframe_client_wasm.js';
import { initProtocol } from './protocol_constants.js';
import { installConsoleRing } from './health.js';
import { createHooks } from './hooks.js';

async function main() {
  // Diagnostics that cost per-call work (console mirroring, per-tile tallies)
  // run only for the e2e suite (`?e2e` on the page URL).
  const diagnostics = new URL(window.location.href).searchParams.has('e2e');
  if (diagnostics) installConsoleRing();
  // wasm-bindgen `--target web`: initialise before any export is called.
  await init();
  // The hooks' recorders read wire constants from Rust; TS must not retype them.
  initProtocol(wasmExports);
  // Errors after this point are shown on the page by the client itself.
  await startClient(createHooks(diagnostics));
}

main().catch((e) => {
  const msg = e instanceof Error ? e.message : String(e);
  const status = document.getElementById('status');
  if (status) status.textContent = `Error: ${msg}`;
  console.error('ghostframe client failed to start', e);
});
