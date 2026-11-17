// SPDX-License-Identifier: MIT OR Apache-2.0
//! The calls the client makes about its own run (events routed, paints,
//! swallowed errors), and the one seam that consumes them. The client queues
//! them while `App` is borrowed and dispatches them with no borrow held.

use crate::app::session::Drained;
use crate::web::route::Routed;

/// One call, queued while `App` is borrowed.
#[derive(Debug, Clone)]
pub enum HookCall {
    Events {
        at_ms: f64,
        records: Vec<Routed>,
    },
    DatagramCounts {
        tile: u32,
        frame: u32,
    },
    RafTick(u32),
    BeforePaint {
        at_ms: f64,
    },
    AfterPaint {
        drained: Drained,
        raf_ticks: u32,
        nack_sent: u32,
    },
    Swallowed {
        site: String,
        message: String,
    },
    RendererError {
        tick: u32,
        message: String,
    },
    ServerStream {
        samples: u32,
        frames: u32,
    },
}

/// What consumes the client's hook calls: the e2e recorder and the page's
/// stats line. Called with no `App` or renderer borrow held.
pub trait HookSink {
    fn dispatch(&self, calls: Vec<HookCall>);
}
