//! The decode thread: turns the net thread's tile payloads into pixels.
//!
//! The net thread runs `ClientNet` in `TileDelivery::Payload` mode, so it
//! does only transport and protocol work -- QUIC, reassembly, validation,
//! coverage, ACKs and NACKs. Decoding a CDF 5/3 tile (bit-plane decode plus
//! an inverse wavelet transform) is the heaviest CPU work in the client, and
//! on the net thread it delayed every timer and acknowledgement behind it.
//! Here it runs beside the transport instead of in its way, on another core.
//!
//! Every core event goes through this thread, not just tiles, so the render
//! thread still sees them in the order the core produced them (paint order
//! between tiles and H.264 frames depends on it). `TileDecoder` is stateful
//! and must see all of them in order anyway.

use std::sync::mpsc::{Receiver, Sender};

use ghostframe_client_core::{Event as CoreEvent, TileDecoder};

use crate::render_thread::RenderMsg;

/// Decode until the net thread drops its end of `rx`.
pub(crate) fn run(rx: Receiver<CoreEvent>, render_tx: Sender<RenderMsg>) {
    let mut decoder = TileDecoder::new();
    let mut render_gone = false;
    while let Ok(ev) = rx.recv() {
        let Some(ev) = decoder.decode(ev) else {
            continue;
        };
        if render_gone {
            continue;
        }
        if render_tx.send(RenderMsg::Core(ev)).is_err() {
            // Keep draining so the net thread never blocks or errors on us;
            // its ACK/NACK timing must keep running until it is told to stop.
            tracing::warn!("render thread channel closed; dropping decoded events");
            render_gone = true;
        }
    }
}
