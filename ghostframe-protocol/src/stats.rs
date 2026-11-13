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

impl StatsSchema {
    /// The whole frame, `[0x01][len u16 LE][payload]`, or why it cannot be
    /// built: a string over its prefix, more than `MAX_DESCRIPTORS`
    /// descriptors, or a payload over [`SERVER_FRAME_MAX_LEN`]. The server
    /// uses this one: a table that outgrows the limits is counted, not a
    /// panic.
    pub fn try_encode_frame(&self) -> Result<Vec<u8>, StatsWireError> {
        if self.metrics.len() > MAX_DESCRIPTORS {
            return Err(StatsWireError::TooLong);
        }
        let mut p = Vec::new();
        p.extend_from_slice(&self.schema_id.to_le_bytes());
        put_str16(&mut p, &self.server_version)?;
        put_str16(&mut p, &self.host_name)?;
        p.extend_from_slice(&(self.metrics.len() as u16).to_le_bytes());
        for m in &self.metrics {
            p.extend_from_slice(&m.id.to_le_bytes());
            put_str8(&mut p, &m.name)?;
            put_str8(&mut p, &m.unit)?;
            p.push(m.kind.as_u8());
            p.push(m.group.as_u8());
            put_str8(&mut p, &m.label)?;
        }
        frame(SERVER_MSG_STATS_SCHEMA, &p)
    }

    /// [`Self::try_encode_frame`] for a schema the caller knows is within
    /// the limits (a test, a static table).
    ///
    /// # Panics
    /// When the schema is over a limit.
    pub fn encode_frame(&self) -> Vec<u8> {
        self.try_encode_frame()
            .expect("stats schema is within the wire limits")
    }

    /// Decode a frame's payload (the bytes after the 3-byte header).
    pub fn decode_payload(payload: &[u8]) -> Result<Self, StatsWireError> {
        let mut c = Cursor::new(payload);
        let schema_id = c.u16()?;
        let server_version = c.str16()?;
        let host_name = c.str16()?;
        let count = usize::from(c.u16()?);
        if count > MAX_DESCRIPTORS {
            return Err(StatsWireError::TooLong);
        }
        let mut metrics = Vec::with_capacity(count);
        for _ in 0..count {
            let id = c.u16()?;
            let name = c.str8()?;
            let unit = c.str8()?;
            let kind = MetricKind::from_u8(c.u8()?).ok_or(StatsWireError::UnknownKind)?;
            let group = MetricGroup::from_u8(c.u8()?);
            let label = c.str8()?;
            metrics.push(MetricDescriptor {
                id,
                name,
                unit,
                kind,
                group,
                label,
            });
        }
        if !c.finished() {
            return Err(StatsWireError::TooLong);
        }
        Ok(Self {
            schema_id,
            server_version,
            host_name,
            metrics,
        })
    }
}

impl StatsSample {
    /// The whole frame, `[0x02][len u16 LE][payload]`, or `TooLong` for a text
    /// value over 255 bytes or a payload over [`SERVER_FRAME_MAX_LEN`].
    pub fn try_encode_frame(&self) -> Result<Vec<u8>, StatsWireError> {
        if self.values.len() > MAX_DESCRIPTORS {
            return Err(StatsWireError::TooLong);
        }
        let mut p = Vec::new();
        p.extend_from_slice(&self.schema_id.to_le_bytes());
        p.extend_from_slice(&self.timestamp_us.to_le_bytes());
        p.extend_from_slice(&(self.values.len() as u16).to_le_bytes());
        for v in &self.values {
            match v {
                MetricValue::Num(x) => p.extend_from_slice(&x.to_bits().to_le_bytes()),
                MetricValue::Text(s) => put_str8(&mut p, s)?,
            }
        }
        frame(SERVER_MSG_STATS_SAMPLE, &p)
    }

    /// [`Self::try_encode_frame`] for a sample the caller knows is within the
    /// limits.
    ///
    /// # Panics
    /// When the sample is over a limit.
    pub fn encode_frame(&self) -> Vec<u8> {
        self.try_encode_frame()
            .expect("stats sample is within the wire limits")
    }

    /// Decode a frame's payload against `schema`, which says each value's
    /// kind. A different `schema_id`, a different value count, or bytes left
    /// over are errors: the panel marks its rows stale instead of showing
    /// misaligned numbers.
    pub fn decode_payload(payload: &[u8], schema: &StatsSchema) -> Result<Self, StatsWireError> {
        let mut c = Cursor::new(payload);
        let schema_id = c.u16()?;
        if schema_id != schema.schema_id {
            return Err(StatsWireError::SchemaMismatch);
        }
        let timestamp_us = c.u64()?;
        let count = usize::from(c.u16()?);
        if count != schema.metrics.len() {
            return Err(StatsWireError::CountMismatch);
        }
        let mut values = Vec::with_capacity(count);
        for m in &schema.metrics {
            values.push(match m.kind {
                MetricKind::Gauge | MetricKind::Counter => MetricValue::Num(c.f64()?),
                MetricKind::Text => MetricValue::Text(c.str8()?),
            });
        }
        if !c.finished() {
            return Err(StatsWireError::CountMismatch);
        }
        Ok(Self {
            schema_id,
            timestamp_us,
            values,
        })
    }
}

/// Reassembles `[type u8][len u16 LE][payload]` frames from stream reads.
///
/// Feed it whatever the stream returned with [`Self::push`] and drain frames
/// with [`Self::next_frame`]. A frame may arrive in pieces, and several may
/// arrive in one read. An unknown type is returned like any other: the caller
/// skips it, and its length keeps the stream in step.
#[derive(Debug, Default)]
pub struct ServerFrameReader {
    buf: Vec<u8>,
}

impl ServerFrameReader {
    /// Append bytes read from the stream.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Bytes held that are not yet a whole frame.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// The next whole frame as `(type, payload)`, `None` when more bytes are
    /// needed.
    ///
    /// A length over [`SERVER_FRAME_MAX_LEN`] returns `Err(TooLong)` once and
    /// empties the buffer. Nothing closes: the stream cannot be resynchronised
    /// (a length is the only marker), so whatever follows is read as new
    /// frames; a server that honours the limit never gets here.
    pub fn next_frame(&mut self) -> Option<Result<(u8, Vec<u8>), StatsWireError>> {
        if self.buf.len() < FRAME_HEADER_LEN {
            return None;
        }
        let len = usize::from(u16::from_le_bytes([self.buf[1], self.buf[2]]));
        if len > SERVER_FRAME_MAX_LEN {
            self.buf.clear();
            return Some(Err(StatsWireError::TooLong));
        }
        let end = FRAME_HEADER_LEN + len;
        if self.buf.len() < end {
            return None;
        }
        let msg_type = self.buf[0];
        let payload = self.buf[FRAME_HEADER_LEN..end].to_vec();
        self.buf.drain(..end);
        Some(Ok((msg_type, payload)))
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

    fn descriptor(id: u16, name: &str, kind: MetricKind, group: MetricGroup) -> MetricDescriptor {
        MetricDescriptor {
            id,
            name: name.to_string(),
            unit: "bit/s".to_string(),
            kind,
            group,
            label: format!("label {name}"),
        }
    }

    fn sample_schema() -> StatsSchema {
        StatsSchema {
            schema_id: 7,
            server_version: "0.1.0 (aab2fa2d, dirty)".to_string(),
            host_name: "evangeline".to_string(),
            metrics: vec![
                descriptor(
                    1,
                    "ghostframe.bwe.estimate",
                    MetricKind::Gauge,
                    MetricGroup::Bandwidth,
                ),
                descriptor(
                    2,
                    "ghostframe.loss.verdicts",
                    MetricKind::Counter,
                    MetricGroup::Loss,
                ),
                descriptor(
                    3,
                    "ghostframe.frame.mode",
                    MetricKind::Text,
                    MetricGroup::Video,
                ),
            ],
        }
    }

    /// A schema with one metric and every string empty: its payload layout is
    /// fixed, so a test can patch a byte by offset.
    ///   0-1 schema_id, 2-3 version len, 4-5 host len, 6-7 count,
    ///   8-9 id, 10 name len, 11 unit len, 12 kind, 13 group, 14 label len.
    fn minimal_payload() -> Vec<u8> {
        let schema = StatsSchema {
            schema_id: 1,
            server_version: String::new(),
            host_name: String::new(),
            metrics: vec![MetricDescriptor {
                id: 9,
                name: String::new(),
                unit: String::new(),
                kind: MetricKind::Gauge,
                group: MetricGroup::Connection,
                label: String::new(),
            }],
        };
        let payload = schema.encode_frame()[FRAME_HEADER_LEN..].to_vec();
        assert_eq!(payload.len(), 15, "layout the offsets below rely on");
        payload
    }

    #[test]
    fn schema_frame_roundtrip_and_framing() {
        let schema = sample_schema();
        let f = schema.encode_frame();
        assert_eq!(f[0], SERVER_MSG_STATS_SCHEMA);
        let len = usize::from(u16::from_le_bytes([f[1], f[2]]));
        assert_eq!(
            len,
            f.len() - FRAME_HEADER_LEN,
            "length covers the payload only"
        );
        assert_eq!(
            StatsSchema::decode_payload(&f[FRAME_HEADER_LEN..]),
            Ok(schema)
        );
    }

    #[test]
    fn every_strict_prefix_of_a_schema_is_an_error() {
        let payload = sample_schema().encode_frame()[FRAME_HEADER_LEN..].to_vec();
        for cut in 0..payload.len() {
            assert!(
                StatsSchema::decode_payload(&payload[..cut]).is_err(),
                "a {cut}-byte prefix of a {}-byte schema decoded",
                payload.len()
            );
        }
    }

    #[test]
    fn schema_unknown_kind_is_an_error_and_unknown_group_is_other() {
        let mut p = minimal_payload();
        p[12] = 9;
        assert_eq!(
            StatsSchema::decode_payload(&p),
            Err(StatsWireError::UnknownKind)
        );
        let mut p = minimal_payload();
        p[13] = 77;
        let s = StatsSchema::decode_payload(&p).expect("an unknown group still decodes");
        assert_eq!(s.metrics[0].group, MetricGroup::Other);
    }

    #[test]
    fn schema_bad_utf8_and_trailing_bytes_are_errors() {
        // One metric named "a": the name byte is at offset 11.
        let schema = StatsSchema {
            schema_id: 1,
            server_version: String::new(),
            host_name: String::new(),
            metrics: vec![descriptor(1, "a", MetricKind::Gauge, MetricGroup::Other)],
        };
        let mut p = schema.encode_frame()[FRAME_HEADER_LEN..].to_vec();
        p[11] = 0xFF;
        assert_eq!(
            StatsSchema::decode_payload(&p),
            Err(StatsWireError::BadUtf8)
        );

        let mut p = minimal_payload();
        p.push(0);
        assert_eq!(
            StatsSchema::decode_payload(&p),
            Err(StatsWireError::TooLong)
        );
    }

    #[test]
    fn schema_descriptor_count_is_bounded_before_it_is_read() {
        let mut p = minimal_payload();
        p[6..8].copy_from_slice(&((MAX_DESCRIPTORS + 1) as u16).to_le_bytes());
        assert_eq!(
            StatsSchema::decode_payload(&p),
            Err(StatsWireError::TooLong)
        );
    }

    #[test]
    fn try_encode_refuses_what_decode_would_refuse() {
        let mut s = sample_schema();
        s.metrics[0].name = "n".repeat(256);
        assert_eq!(s.try_encode_frame(), Err(StatsWireError::TooLong));

        let mut s = sample_schema();
        s.metrics = (0..=MAX_DESCRIPTORS as u16)
            .map(|i| descriptor(i, "m", MetricKind::Gauge, MetricGroup::Other))
            .collect();
        assert_eq!(
            s.try_encode_frame(),
            Err(StatsWireError::TooLong),
            "513 descriptors"
        );

        // Every string is within its own 255-byte prefix, so only the
        // 16 KiB frame limit can refuse this one. Built directly, not through
        // `descriptor()`, whose label would push a 255-byte name past 255.
        let mut s = sample_schema();
        s.metrics = (0..100u16)
            .map(|i| MetricDescriptor {
                id: i,
                name: "n".repeat(200),
                unit: "u".to_string(),
                kind: MetricKind::Gauge,
                group: MetricGroup::Other,
                label: "l".repeat(200),
            })
            .collect();
        assert_eq!(
            s.try_encode_frame(),
            Err(StatsWireError::TooLong),
            "100 descriptors of about 410 bytes (about 41 KB) are over the 16 KiB frame limit"
        );
        // Control: the same shape with 10 descriptors (about 4 KB) encodes,
        // so the refusal above is the size and not a string.
        s.metrics.truncate(10);
        assert!(s.try_encode_frame().is_ok());
    }

    fn sample_for(schema: &StatsSchema) -> StatsSample {
        StatsSample {
            schema_id: schema.schema_id,
            timestamp_us: 123_456_789,
            values: vec![
                MetricValue::Num(42_100_000.0),
                MetricValue::Num(17.0),
                MetricValue::Text("h264".to_string()),
            ],
        }
    }

    #[test]
    fn sample_frame_roundtrip_and_framing() {
        let schema = sample_schema();
        let sample = sample_for(&schema);
        let f = sample.encode_frame();
        assert_eq!(f[0], SERVER_MSG_STATS_SAMPLE);
        let len = usize::from(u16::from_le_bytes([f[1], f[2]]));
        assert_eq!(len, f.len() - FRAME_HEADER_LEN);
        assert_eq!(
            StatsSample::decode_payload(&f[FRAME_HEADER_LEN..], &schema),
            Ok(sample)
        );
    }

    #[test]
    fn sample_with_the_wrong_schema_id_is_rejected() {
        let schema = sample_schema();
        let mut sample = sample_for(&schema);
        sample.schema_id = schema.schema_id.wrapping_add(1);
        let f = sample.encode_frame();
        assert_eq!(
            StatsSample::decode_payload(&f[FRAME_HEADER_LEN..], &schema),
            Err(StatsWireError::SchemaMismatch)
        );
    }

    #[test]
    fn sample_with_the_wrong_value_count_is_rejected() {
        let schema = sample_schema();
        let mut short = sample_for(&schema);
        short.values.pop();
        let f = short.encode_frame();
        assert_eq!(
            StatsSample::decode_payload(&f[FRAME_HEADER_LEN..], &schema),
            Err(StatsWireError::CountMismatch)
        );
        let mut long = sample_for(&schema);
        long.values.push(MetricValue::Num(1.0));
        let f = long.encode_frame();
        assert_eq!(
            StatsSample::decode_payload(&f[FRAME_HEADER_LEN..], &schema),
            Err(StatsWireError::CountMismatch)
        );
    }

    #[test]
    fn every_strict_prefix_of_a_sample_is_an_error() {
        let schema = sample_schema();
        let payload = sample_for(&schema).encode_frame()[FRAME_HEADER_LEN..].to_vec();
        for cut in 0..payload.len() {
            assert!(
                StatsSample::decode_payload(&payload[..cut], &schema).is_err(),
                "a {cut}-byte prefix of a {}-byte sample decoded",
                payload.len()
            );
        }
    }

    #[test]
    fn sample_trailing_bytes_and_bad_utf8_are_errors() {
        let schema = sample_schema();
        let mut p = sample_for(&schema).encode_frame()[FRAME_HEADER_LEN..].to_vec();
        p.push(0);
        assert_eq!(
            StatsSample::decode_payload(&p, &schema),
            Err(StatsWireError::CountMismatch)
        );
        // The text value "h264" is last: its bytes are the final four.
        let mut p = sample_for(&schema).encode_frame()[FRAME_HEADER_LEN..].to_vec();
        let n = p.len();
        p[n - 1] = 0xFF;
        assert_eq!(
            StatsSample::decode_payload(&p, &schema),
            Err(StatsWireError::BadUtf8)
        );
    }

    #[test]
    fn sample_text_over_255_bytes_cannot_be_encoded() {
        let s = StatsSample {
            schema_id: 1,
            timestamp_us: 0,
            values: vec![MetricValue::Text("x".repeat(256))],
        };
        assert_eq!(s.try_encode_frame(), Err(StatsWireError::TooLong));
    }

    #[test]
    fn sample_over_the_frame_limit_cannot_be_encoded() {
        // 70 values of 255 bytes are 70 x 256 + 12 = 17 932 bytes: every
        // string fits its prefix and the count is far under 512, so only the
        // 16 KiB frame limit refuses it.
        let s = StatsSample {
            schema_id: 1,
            timestamp_us: 0,
            values: (0..70)
                .map(|_| MetricValue::Text("x".repeat(255)))
                .collect(),
        };
        assert_eq!(s.try_encode_frame(), Err(StatsWireError::TooLong));
        let mut ok = s.clone();
        ok.values.truncate(10);
        assert!(ok.try_encode_frame().is_ok());
    }

    fn two_frames() -> (Vec<u8>, Vec<u8>) {
        let schema = sample_schema();
        (schema.encode_frame(), sample_for(&schema).encode_frame())
    }

    #[test]
    fn reader_returns_nothing_until_a_frame_is_whole() {
        let (a, _) = two_frames();
        let mut r = ServerFrameReader::default();
        assert!(r.next_frame().is_none(), "empty");
        for b in &a[..a.len() - 1] {
            r.push(&[*b]);
            assert!(r.next_frame().is_none(), "a frame must not appear early");
        }
        r.push(&a[a.len() - 1..]);
        let (ty, payload) = r.next_frame().expect("whole").expect("valid");
        assert_eq!(ty, SERVER_MSG_STATS_SCHEMA);
        assert_eq!(payload, a[FRAME_HEADER_LEN..]);
        assert!(r.next_frame().is_none());
    }

    #[test]
    fn reader_splits_frames_joined_in_one_read() {
        let (a, b) = two_frames();
        let mut r = ServerFrameReader::default();
        let mut joined = a.clone();
        joined.extend_from_slice(&b);
        r.push(&joined);
        assert_eq!(
            r.next_frame().expect("first").expect("ok").0,
            SERVER_MSG_STATS_SCHEMA
        );
        assert_eq!(
            r.next_frame().expect("second").expect("ok").0,
            SERVER_MSG_STATS_SAMPLE
        );
        assert!(r.next_frame().is_none());
        assert_eq!(r.buffered(), 0);
    }

    #[test]
    fn reader_passes_an_unknown_type_through_by_its_length() {
        let mut r = ServerFrameReader::default();
        r.push(&[0x7E, 2, 0, 0xAA, 0xBB]);
        let (_, b) = two_frames();
        r.push(&b);
        assert_eq!(
            r.next_frame().expect("unknown").expect("ok"),
            (0x7E, vec![0xAA, 0xBB])
        );
        assert_eq!(
            r.next_frame().expect("then the real one").expect("ok").0,
            SERVER_MSG_STATS_SAMPLE,
            "the unknown frame was skipped by its length, not by guessing"
        );
    }

    #[test]
    fn reader_reports_an_oversize_length_once_and_resets() {
        let mut r = ServerFrameReader::default();
        let too_long = (SERVER_FRAME_MAX_LEN as u16 + 1).to_le_bytes();
        r.push(&[SERVER_MSG_STATS_SAMPLE, too_long[0], too_long[1], 1, 2, 3]);
        assert_eq!(r.next_frame(), Some(Err(StatsWireError::TooLong)));
        assert_eq!(r.buffered(), 0, "the reader state is dropped");
        assert!(r.next_frame().is_none(), "reported once, not on every poll");
        // A length at the limit is fine.
        let at_limit = (SERVER_FRAME_MAX_LEN as u16).to_le_bytes();
        r.push(&[SERVER_MSG_STATS_SAMPLE, at_limit[0], at_limit[1]]);
        assert!(
            r.next_frame().is_none(),
            "waiting for the payload, not an error"
        );
    }
}
