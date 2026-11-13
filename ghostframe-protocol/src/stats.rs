//! The connection-stats wire format: how a client asks the server for its
//! numbers, and how the server sends them.
//!
//! ## Channels
//!
//! *Client to server* the subscribe is a feedback-stream message
//! ([`StatsSubscribe`], type [`crate::feedback_stream::STATS_SUBSCRIBE_MSG_TYPE`]).
//!
//! *Server to client* the numbers travel on the return half of that same
//! bidirectional stream, which nothing used before. It has its own one-byte
//! type namespace ([`SERVER_MSG_STATS_SCHEMA`], [`SERVER_MSG_STATS_SAMPLE`])
//! and explicit framing, `[type u8][len u16 LE][payload]`, because a stream
//! read can split or join messages. [`ServerFrameReader`] reassembles.
//!
//! A frame's payload is at most [`SERVER_FRAME_MAX_LEN`]. Every string and
//! count is bounded and checked on decode; decoding never panics.
//!
//! ## Layout
//!
//! ```text
//! STATS_SCHEMA   schema_id u16 | server_version (u16 len + utf8)
//!                | host_name (u16 len + utf8) | count u16 (<= 512)
//!                | count x [ id u16 | name (u8 len + utf8) | unit (u8 len + utf8)
//!                            | kind u8 | group u8 | label (u8 len + utf8) ]
//! STATS_SAMPLE   schema_id u16 | timestamp_us u64 | count u16 | count x value
//!                value: gauge or counter -> f64 LE; text -> u8 len + utf8
//!                (the kind comes from the schema, not the wire)
//! ```
//!
//! No capability bit and no compatibility path: the web client is embedded in
//! the server binary, so both sides always ship together.

use thiserror::Error;

use crate::feedback_stream::{STATS_SUBSCRIBE_MSG_TYPE, STATS_SUBSCRIBE_SIZE};

/// Server-to-client frame type: the metric list.
pub const SERVER_MSG_STATS_SCHEMA: u8 = 0x01;
/// Server-to-client frame type: one value per descriptor.
pub const SERVER_MSG_STATS_SAMPLE: u8 = 0x02;
/// Largest payload of one server-to-client frame.
pub const SERVER_FRAME_MAX_LEN: usize = 16 * 1024;
/// Most descriptors one schema may carry.
pub(crate) const MAX_DESCRIPTORS: usize = 512;
/// Bytes in a frame header: type and length.
const FRAME_HEADER_LEN: usize = 3;
/// Longest string behind a one-byte length prefix.
const MAX_SHORT_STR: usize = 255;

/// Why a stats message did not decode, or could not be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum StatsWireError {
    /// The buffer ended before the message did.
    #[error("stats message truncated")]
    Truncated,
    /// A string, a count or a whole frame is over its limit; also bytes left
    /// over after a schema.
    #[error("stats message or field over its limit")]
    TooLong,
    /// A descriptor's kind byte is not a [`MetricKind`].
    #[error("unknown metric kind")]
    UnknownKind,
    /// A sample's value count differs from the schema's descriptor count (or
    /// bytes are left over after its values).
    #[error("sample value count does not match the schema")]
    CountMismatch,
    /// A sample names a different schema than the one it is decoded against.
    #[error("sample schema_id does not match the schema")]
    SchemaMismatch,
    /// A string is not UTF-8.
    #[error("string is not valid UTF-8")]
    BadUtf8,
    /// A subscribe message with the wrong type byte or an `on` other than 0 or 1.
    #[error("not a stats subscribe message")]
    BadMessage,
}

/// How a metric behaves over time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    /// A value that goes up and down.
    Gauge = 0,
    /// A running total; the reader derives a rate.
    Counter = 1,
    /// A string.
    Text = 2,
}

impl MetricKind {
    /// `None` for a byte that is not a kind.
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Gauge),
            1 => Some(Self::Counter),
            2 => Some(Self::Text),
            _ => None,
        }
    }

    /// The wire byte.
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// The panel section a metric belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricGroup {
    Connection = 0,
    Bandwidth = 1,
    Video = 2,
    Loss = 3,
    /// Any group this build does not know: a metric a newer server adds still
    /// shows up, in a trailing section.
    Other = 255,
}

impl MetricGroup {
    /// An unknown byte is [`MetricGroup::Other`], never an error.
    pub fn from_u8(b: u8) -> Self {
        match b {
            0 => Self::Connection,
            1 => Self::Bandwidth,
            2 => Self::Video,
            3 => Self::Loss,
            _ => Self::Other,
        }
    }

    /// The wire byte.
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// One metric the server can report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricDescriptor {
    /// Stable within a schema; the position in a sample is the schema order,
    /// not this id.
    pub id: u16,
    /// OTel-style name, e.g. `ghostframe.bwe.estimate`.
    pub name: String,
    /// UCUM unit: `bit/s`, `ms`, `1`, `By`, `s`.
    pub unit: String,
    pub kind: MetricKind,
    pub group: MetricGroup,
    /// What the panel shows.
    pub label: String,
}

/// The metric list a session is about to send samples against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatsSchema {
    /// Changes whenever a schema is (re)sent; a sample names the schema it
    /// was built against.
    pub schema_id: u16,
    pub server_version: String,
    pub host_name: String,
    pub metrics: Vec<MetricDescriptor>,
}

/// One value of one metric.
#[derive(Debug, Clone, PartialEq)]
pub enum MetricValue {
    Num(f64),
    Text(String),
}

/// One second's values, in schema order.
#[derive(Debug, Clone, PartialEq)]
pub struct StatsSample {
    pub schema_id: u16,
    /// Server monotonic clock, microseconds.
    pub timestamp_us: u64,
    pub values: Vec<MetricValue>,
}

/// Start (`on`) or stop the stats stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatsSubscribe {
    pub on: bool,
}

impl StatsSubscribe {
    /// `[STATS_SUBSCRIBE_MSG_TYPE, on]`.
    pub fn encode(&self) -> [u8; STATS_SUBSCRIBE_SIZE] {
        [STATS_SUBSCRIBE_MSG_TYPE, u8::from(self.on)]
    }

    /// Bytes past [`STATS_SUBSCRIBE_SIZE`] belong to the next message and are
    /// not examined.
    pub fn decode(data: &[u8]) -> Result<Self, StatsWireError> {
        if data.len() < STATS_SUBSCRIBE_SIZE {
            return Err(StatsWireError::Truncated);
        }
        if data[0] != STATS_SUBSCRIBE_MSG_TYPE {
            return Err(StatsWireError::BadMessage);
        }
        match data[1] {
            0 => Ok(Self { on: false }),
            1 => Ok(Self { on: true }),
            _ => Err(StatsWireError::BadMessage),
        }
    }
}

// ---- encoding helpers --------------------------------------------------

/// A string behind a one-byte length.
fn put_str8(out: &mut Vec<u8>, s: &str) -> Result<(), StatsWireError> {
    if s.len() > MAX_SHORT_STR {
        return Err(StatsWireError::TooLong);
    }
    out.push(s.len() as u8);
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

/// A string behind a two-byte length.
fn put_str16(out: &mut Vec<u8>, s: &str) -> Result<(), StatsWireError> {
    let len = u16::try_from(s.len()).map_err(|_| StatsWireError::TooLong)?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

/// `[type][len u16 LE][payload]`, or `TooLong` past [`SERVER_FRAME_MAX_LEN`].
fn frame(msg_type: u8, payload: &[u8]) -> Result<Vec<u8>, StatsWireError> {
    if payload.len() > SERVER_FRAME_MAX_LEN {
        return Err(StatsWireError::TooLong);
    }
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    out.push(msg_type);
    out.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// A bounds-checked reader over a payload.
struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], StatsWireError> {
        let end = self.pos.checked_add(n).ok_or(StatsWireError::Truncated)?;
        let s = self
            .buf
            .get(self.pos..end)
            .ok_or(StatsWireError::Truncated)?;
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, StatsWireError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, StatsWireError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u64(&mut self) -> Result<u64, StatsWireError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes(
            b.try_into().expect("take(8) is 8 bytes"),
        ))
    }

    fn f64(&mut self) -> Result<f64, StatsWireError> {
        Ok(f64::from_bits(self.u64()?))
    }

    fn str8(&mut self) -> Result<String, StatsWireError> {
        let n = usize::from(self.u8()?);
        let b = self.take(n)?;
        String::from_utf8(b.to_vec()).map_err(|_| StatsWireError::BadUtf8)
    }

    fn str16(&mut self) -> Result<String, StatsWireError> {
        let n = usize::from(self.u16()?);
        let b = self.take(n)?;
        String::from_utf8(b.to_vec()).map_err(|_| StatsWireError::BadUtf8)
    }

    fn finished(&self) -> bool {
        self.pos == self.buf.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscribe_roundtrip_and_layout() {
        for on in [false, true] {
            let bytes = StatsSubscribe { on }.encode();
            assert_eq!(bytes, [0x0B, u8::from(on)]);
            assert_eq!(StatsSubscribe::decode(&bytes), Ok(StatsSubscribe { on }));
        }
    }

    #[test]
    fn subscribe_hostile_input() {
        assert_eq!(StatsSubscribe::decode(&[]), Err(StatsWireError::Truncated));
        assert_eq!(
            StatsSubscribe::decode(&[0x0B]),
            Err(StatsWireError::Truncated)
        );
        assert_eq!(
            StatsSubscribe::decode(&[0x0C, 1]),
            Err(StatsWireError::BadMessage),
            "a wrong type byte"
        );
        assert_eq!(
            StatsSubscribe::decode(&[0x0B, 2]),
            Err(StatsWireError::BadMessage),
            "`on` is 0 or 1"
        );
        // The stream is framed by message type: bytes past the message belong
        // to the next one and are not examined.
        assert_eq!(
            StatsSubscribe::decode(&[0x0B, 1, 0xFF, 0xFF]),
            Ok(StatsSubscribe { on: true })
        );
    }

    #[test]
    fn metric_group_unknown_byte_decodes_to_other() {
        for g in [
            MetricGroup::Connection,
            MetricGroup::Bandwidth,
            MetricGroup::Video,
            MetricGroup::Loss,
            MetricGroup::Other,
        ] {
            assert_eq!(MetricGroup::from_u8(g.as_u8()), g);
        }
        assert_eq!(MetricGroup::from_u8(4), MetricGroup::Other);
        assert_eq!(MetricGroup::from_u8(77), MetricGroup::Other);
        assert_eq!(MetricGroup::Other.as_u8(), 255);
    }

    #[test]
    fn metric_kind_unknown_byte_is_not_a_kind() {
        for k in [MetricKind::Gauge, MetricKind::Counter, MetricKind::Text] {
            assert_eq!(MetricKind::from_u8(k.as_u8()), Some(k));
        }
        assert_eq!(MetricKind::from_u8(3), None);
        assert_eq!(MetricKind::from_u8(255), None);
    }
}
