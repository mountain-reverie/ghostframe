//! Input **encoding** only. Capture is platform-bound and stays in
//! `src/input/wire.ts`; this is the half that moves.

use ghostframe_client_core::input;
use wasm_bindgen::prelude::*;

#[wasm_bindgen(js_name = encodePointerMove)]
pub fn encode_pointer_move(x: i16, y: i16) -> Vec<u8> {
    input::encode_pointer_move(x, y).to_vec()
}

#[wasm_bindgen(js_name = encodePointerButton)]
pub fn encode_pointer_button(x: i16, y: i16, button: u8, down: bool) -> Vec<u8> {
    input::encode_pointer_button(x, y, button, down).to_vec()
}

#[wasm_bindgen(js_name = encodeWheel)]
pub fn encode_wheel(dx: i16, dy: i16) -> Vec<u8> {
    input::encode_wheel(dx, dy).to_vec()
}

#[wasm_bindgen(js_name = encodeKeyDown)]
pub fn encode_key_down(keysym: u32) -> Vec<u8> {
    input::encode_key_down(keysym).to_vec()
}

#[wasm_bindgen(js_name = encodeKeyUp)]
pub fn encode_key_up(keysym: u32) -> Vec<u8> {
    input::encode_key_up(keysym).to_vec()
}

/// `undefined` for keys with no keysym mapping — the caller drops the event.
#[wasm_bindgen(js_name = keyToKeysym)]
pub fn key_to_keysym(key: &str) -> Option<u32> {
    input::key_to_keysym(key)
}
