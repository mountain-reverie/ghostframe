//! A stats subscriber for the browserless harness: sends `STATS_SUBSCRIBE`
//! through the scene's real `ClientNet`, reads what the server writes on the
//! feedback stream's return half, and decodes it with the protocol crate.
//!
//! It exists so the server half of the stats panel is testable with no UI:
//! the numbers a scene reads here crossed the real QUIC stream, the real
//! framing and the real decoder.

use ghostframe_client_net::ClientNet;
use ghostframe_protocol::stats::{
    ServerFrameReader, StatsSample, StatsSchema, StatsSubscribe, SERVER_MSG_STATS_SAMPLE,
    SERVER_MSG_STATS_SCHEMA,
};
use std::time::Duration;

/// When the scene's client subscribes and unsubscribes, as offsets from the
/// scene's start. `None` everywhere is a listener that never subscribes: the
/// control for "nothing is sent before a subscribe".
#[derive(Debug, Clone, Copy, Default)]
pub struct StatsPlan {
    pub subscribe_at: Option<Duration>,
    /// Subscribe again (a repeated `on`).
    pub resubscribe_at: Option<Duration>,
    pub unsubscribe_at: Option<Duration>,
}

/// What the tap saw, in virtual microseconds since the scene started.
#[derive(Debug, Clone, Default)]
pub struct StatsProbe {
    pub subscribes_sent: u32,
    pub unsubscribes_sent: u32,
    /// `(arrival_us, schema)`.
    pub schemas: Vec<(u64, StatsSchema)>,
    /// `(arrival_us, sample)`, decoded against the schema current at arrival.
    pub samples: Vec<(u64, StatsSample)>,
    /// Samples that arrived before a schema or did not match it.
    pub rejected: u32,
    /// Frames that did not parse (oversize, bad schema payload).
    pub frame_errors: u32,
    /// Bytes read off the return half in total: non-zero proves the read path
    /// ran, which a "nothing arrived" assertion needs.
    pub bytes_read: u64,
}

pub struct StatsTap {
    plan: StatsPlan,
    reader: ServerFrameReader,
    current: Option<StatsSchema>,
    probe: StatsProbe,
    sent_on: bool,
    sent_again: bool,
    sent_off: bool,
}

fn due(at: Option<Duration>, vt_us: u64) -> bool {
    at.is_some_and(|d| vt_us >= d.as_micros() as u64)
}

impl StatsTap {
    pub fn new(plan: StatsPlan) -> Self {
        Self {
            plan,
            reader: ServerFrameReader::default(),
            current: None,
            probe: StatsProbe::default(),
            sent_on: false,
            sent_again: false,
            sent_off: false,
        }
    }

    /// Send what is due and decode what arrived. Call once per harness
    /// iteration with the current virtual time.
    pub fn step(&mut self, client: &mut ClientNet, vt_us: u64) {
        if !self.sent_on && due(self.plan.subscribe_at, vt_us) {
            self.sent_on = true;
            self.probe.subscribes_sent += 1;
            client.send_input(StatsSubscribe { on: true }.encode().to_vec(), vt_us);
        }
        if self.sent_on && !self.sent_again && due(self.plan.resubscribe_at, vt_us) {
            self.sent_again = true;
            self.probe.subscribes_sent += 1;
            client.send_input(StatsSubscribe { on: true }.encode().to_vec(), vt_us);
        }
        if self.sent_on && !self.sent_off && due(self.plan.unsubscribe_at, vt_us) {
            self.sent_off = true;
            self.probe.unsubscribes_sent += 1;
            client.send_input(StatsSubscribe { on: false }.encode().to_vec(), vt_us);
        }

        let bytes = client.read_feedback_return(vt_us);
        self.probe.bytes_read += bytes.len() as u64;
        self.reader.push(&bytes);
        while let Some(frame) = self.reader.next_frame() {
            let (msg_type, payload) = match frame {
                Ok(f) => f,
                Err(_) => {
                    self.probe.frame_errors += 1;
                    continue;
                }
            };
            match msg_type {
                SERVER_MSG_STATS_SCHEMA => match StatsSchema::decode_payload(&payload) {
                    Ok(schema) => {
                        self.current = Some(schema.clone());
                        self.probe.schemas.push((vt_us, schema));
                    }
                    Err(_) => self.probe.frame_errors += 1,
                },
                SERVER_MSG_STATS_SAMPLE => match self.current.as_ref() {
                    Some(schema) => match StatsSample::decode_payload(&payload, schema) {
                        Ok(sample) => self.probe.samples.push((vt_us, sample)),
                        Err(_) => self.probe.rejected += 1,
                    },
                    None => self.probe.rejected += 1,
                },
                // An unknown server frame type is skipped by its length.
                _ => {}
            }
        }
    }

    pub fn finish(self) -> StatsProbe {
        self.probe
    }
}
