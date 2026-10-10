// Initialises the process-wide protocol constants from the nodejs wasm build
// before every suite, the way main() does from pkg-web in the browser.
import * as wasm from '../pkg-node/ghostframe_client_wasm.js';
import { initProtocol } from '../src/protocol_constants';

initProtocol(wasm as never);
