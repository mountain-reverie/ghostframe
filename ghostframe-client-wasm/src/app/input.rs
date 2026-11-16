// SPDX-License-Identifier: MIT OR Apache-2.0
//! Input capture (port of `input/wire.ts`'s `attachInputCapture`): pointer,
//! wheel and keyboard events on the canvas, encoded by
//! `ghostframe_client_core::input` and sent on the feedback stream.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use ghostframe_client_core::input::{
    encode_key_down, encode_key_up, encode_pointer_button, encode_pointer_move, encode_wheel,
    key_to_keysym,
};
use wasm_bindgen::convert::FromWasmAbi;
use wasm_bindgen::prelude::*;
use web_sys::{
    AddEventListenerOptions, HtmlCanvasElement, KeyboardEvent, PointerEvent, WheelEvent,
};

use crate::app::input_map::{
    canvas_to_fb, dom_button_to_x11, wheel_ticks, CanvasRect, POINTER_LEFT,
};
use crate::app::streams::LoggedWriter;
use crate::web::renderer::WebRenderer;

/// Registers `f` for `ty` on the canvas. The closure is forgotten: every
/// listener here lives as long as the page, which is as long as the canvas.
fn listen<E: FromWasmAbi + 'static>(
    canvas: &HtmlCanvasElement,
    ty: &str,
    passive: Option<bool>,
    f: impl FnMut(E) + 'static,
) {
    let closure = Closure::<dyn FnMut(E)>::new(f);
    let function = closure.as_ref().unchecked_ref();
    let _ = match passive {
        None => canvas.add_event_listener_with_callback(ty, function),
        Some(p) => {
            let opts = AddEventListenerOptions::new();
            opts.set_passive(p);
            canvas.add_event_listener_with_callback_and_add_event_listener_options(
                ty, function, &opts,
            )
        }
    };
    // Page-lifetime listener: dropping the closure would make the browser call
    // a freed function, so it is leaked on purpose.
    closure.forget();
}

/// `clientX` / `clientY` as the doubles the browser reports: web-sys's
/// `client_x()` returns an `i32`, which truncates before the canvas scale and
/// so moves the pointer by up to one CSS pixel times that scale.
fn client_xy(e: &PointerEvent) -> (f64, f64) {
    let read = |name: &str, fallback: i32| {
        js_sys::Reflect::get(e, &JsValue::from_str(name))
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(f64::from(fallback))
    };
    (read("clientX", e.client_x()), read("clientY", e.client_y()))
}

pub fn attach_input(
    canvas: &HtmlCanvasElement,
    send: Rc<LoggedWriter>,
    renderer: Rc<RefCell<WebRenderer>>,
) {
    // Allow keyboard focus.
    canvas.set_tab_index(0);

    // The pointer position in framebuffer pixels. A renderer that is busy
    // (cannot happen between steps, but must not panic) reads as no frame.
    let to_fb = {
        let canvas = canvas.clone();
        let renderer = renderer.clone();
        move |e: &PointerEvent| -> (i16, i16) {
            let (client_x, client_y) = client_xy(e);
            let r = canvas.get_bounding_client_rect();
            let rect = CanvasRect {
                left: r.left(),
                top: r.top(),
                width: r.width(),
                height: r.height(),
            };
            let dims = renderer
                .try_borrow()
                .map_or((0, 0), |r| (r.fb_width(), r.fb_height()));
            canvas_to_fb(client_x, client_y, rect, dims)
        }
    };

    // Pointer moves are coalesced to one per animation frame (30 to 60 Hz): the
    // latest position waits in `pending` and one persistent rAF callback sends it.
    let pending: Rc<Cell<Option<(i16, i16)>>> = Rc::new(Cell::new(None));
    let queued = Rc::new(Cell::new(false));
    let flush = {
        let (pending, queued, send) = (pending.clone(), queued.clone(), send.clone());
        Rc::new(Closure::<dyn FnMut()>::new(move || {
            queued.set(false);
            if let Some((x, y)) = pending.take() {
                send.write(&encode_pointer_move(x, y));
            }
        }))
    };

    {
        let to_fb = to_fb.clone();
        listen(canvas, "pointermove", None, move |e: PointerEvent| {
            pending.set(Some(to_fb(&e)));
            if !queued.replace(true) {
                if let Some(w) = web_sys::window() {
                    if w.request_animation_frame((*flush).as_ref().unchecked_ref())
                        .is_err()
                    {
                        queued.set(false);
                    }
                }
            }
        });
    }

    {
        let (send, to_fb, canvas2) = (send.clone(), to_fb.clone(), canvas.clone());
        listen(canvas, "pointerdown", None, move |e: PointerEvent| {
            let _ = canvas2.focus();
            let Some(btn) = dom_button_to_x11(e.button()) else {
                return;
            };
            let (x, y) = to_fb(&e);
            send.write(&encode_pointer_button(x, y, btn, true));
            e.prevent_default();
        });
    }

    {
        let (send, to_fb) = (send.clone(), to_fb.clone());
        listen(canvas, "pointerup", None, move |e: PointerEvent| {
            let Some(btn) = dom_button_to_x11(e.button()) else {
                return;
            };
            let (x, y) = to_fb(&e);
            send.write(&encode_pointer_button(x, y, btn, false));
            e.prevent_default();
        });
    }

    {
        // Marker so the server can hide its cursor while the user is in the
        // browser UI.
        let send = send.clone();
        listen(canvas, "pointerleave", None, move |_: PointerEvent| {
            send.write(&encode_pointer_move(POINTER_LEFT.0, POINTER_LEFT.1));
        });
    }

    {
        // Not passive: `prevent_default` must stop the page from scrolling.
        let send = send.clone();
        listen(canvas, "wheel", Some(false), move |e: WheelEvent| {
            if let Some((dx, dy)) = wheel_ticks(e.delta_x(), e.delta_y()) {
                send.write(&encode_wheel(dx, dy));
            }
            e.prevent_default();
        });
    }

    {
        let send = send.clone();
        listen(canvas, "keydown", None, move |e: KeyboardEvent| {
            if let Some(sym) = key_to_keysym(&e.key()) {
                send.write(&encode_key_down(sym));
                e.prevent_default();
            }
        });
    }

    listen(canvas, "keyup", None, move |e: KeyboardEvent| {
        if let Some(sym) = key_to_keysym(&e.key()) {
            send.write(&encode_key_up(sym));
            e.prevent_default();
        }
    });
}
