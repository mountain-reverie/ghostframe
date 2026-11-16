// SPDX-License-Identifier: MIT OR Apache-2.0
//! The real `VideoDecoder` through `FrameDecoder<WebCodec>`: the frame_seq of an
//! access unit comes back on its picture, and the core hears "decoded".
#![cfg(target_arch = "wasm32")]

use std::cell::RefCell;
use std::rc::Rc;

use ghostframe_client_core::H264Outcome;
use ghostframe_client_wasm::app::decoder::FrameDecoder;
use ghostframe_client_wasm::app::webcodecs::{frame_seq_of, WebCodec};
use ghostframe_client_wasm::web::raw::close_frame;
use js_sys::{Function, Promise};
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

const PROBE_CLIP: &[u8] = include_bytes!("../../ghostframe-client-h264/src/probe_clip.h264");

#[wasm_bindgen_test]
async fn the_real_decoder_decodes_the_probe_clip() {
    let outputs: Rc<RefCell<Vec<(u32, u32)>>> = Rc::default();
    let errors: Rc<RefCell<Vec<String>>> = Rc::default();
    let resolve: Rc<RefCell<Option<Function>>> = Rc::default();

    let wake = {
        let resolve = resolve.clone();
        move || {
            if let Some(f) = resolve.borrow().as_ref() {
                let _ = f.call0(&JsValue::NULL);
            }
        }
    };
    let on_output = {
        let (outputs, wake) = (outputs.clone(), wake.clone());
        Rc::new(move |generation: u32, frame: JsValue| {
            outputs
                .borrow_mut()
                .push((generation, frame_seq_of(&frame)));
            close_frame(&frame);
            wake();
        })
    };
    let on_error = {
        let errors = errors.clone();
        Rc::new(move |_generation: u32, message: String| {
            errors.borrow_mut().push(message);
            wake();
        })
    };

    let mut decoder = FrameDecoder::new(WebCodec::new(on_output, on_error))
        .expect("the browser's VideoDecoder configures");

    // The promise is created before the decode so a fast callback finds `resolve`.
    let resolve_slot = resolve.clone();
    let done = Promise::new(&mut |res, _| {
        *resolve_slot.borrow_mut() = Some(res);
    });
    decoder
        .decode(PROBE_CLIP, true, 77)
        .expect("decode accepts the key chunk");
    // A decoder may hold a lone key chunk's picture until more input or a
    // flush (in a session the next access unit pushes it out): flush it.
    let flushed = decoder.codec().flush().expect("a decoder is open");
    // Not awaited here: a decoder that never answers would hang the run; the
    // race below has the timeout.
    wasm_bindgen_futures::spawn_local(async move {
        let _ = JsFuture::from(flushed).await;
    });

    let timeout = Promise::new(&mut |_, rej| {
        let reject =
            Function::new_with_args("rej", "setTimeout(() => rej(new Error('timeout')), 10000)");
        let _ = reject.call1(&JsValue::NULL, &rej);
    });
    let answered = JsFuture::from(Promise::race(&js_sys::Array::of2(&done, &timeout)))
        .await
        .map_err(|e| ghostframe_client_wasm::web::raw::js_message(&e));
    if let Err(e) = answered {
        // Say whether this browser decodes the clip's profile at all (gate G2).
        let probe = Function::new_no_args(
            "return VideoDecoder.isConfigSupported({ codec: 'avc1.64000a', codedWidth: 64, codedHeight: 64 })\
               .then(r => JSON.stringify({ supported: r.supported }), e => 'isConfigSupported threw: ' + e);",
        );
        let support = match probe.call0(&JsValue::NULL) {
            Ok(p) => JsFuture::from(Promise::from(p))
                .await
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_default(),
            Err(e) => ghostframe_client_wasm::web::raw::js_message(&e),
        };
        panic!(
            "the decoder did not answer within 10 s ({e}); errors {:?}; {support}",
            errors.borrow()
        );
    }

    if let Some(e) = errors.borrow().first() {
        panic!("decoder error: {e}");
    }
    let out = outputs.borrow().clone();
    assert_eq!(out.len(), 1, "one picture: {out:?}");
    let (generation, frame_seq) = out[0];
    assert_eq!(frame_seq, 77);
    assert!(decoder.on_output(generation, frame_seq));
    assert_eq!(decoder.codec_string(), "avc1.64000a");
    assert_eq!(decoder.take_notes(), vec![(77, H264Outcome::Decoded)]);
}
