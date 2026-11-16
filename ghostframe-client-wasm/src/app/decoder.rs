// SPDX-License-Identifier: MIT OR Apache-2.0
//! The decisions of the WebCodecs `VideoDecoder` wrapper (port of
//! `decoder.ts`): what to tell the core about every access unit, when to
//! rebuild the decoder after a failure, and which codec string to configure.
//! `webcodecs.rs` is the browser half.
//!
//! The browser never calls `output` or `error` from inside `decode()`,
//! `configure()` or `close()`; both arrive as their own tasks, tagged with the
//! generation `open` was given, so a late callback from a replaced decoder is
//! recognised and ignored.

use std::collections::VecDeque;

use ghostframe_client_core::H264Outcome;

/// What a decoder is configured with before the first SPS says better.
pub const DEFAULT_CODEC: &str = "avc1.64002a";
/// The coded size the decoder is configured with (`main.ts` passed 1920x1080;
/// WebCodecs takes the real size from the stream).
pub const CODED_SIZE: (u32, u32) = (1920, 1080);

/// The WebCodecs codec string (`avc1.PPCCLL`) of an Annex-B access unit, read
/// from its SPS: profile_idc, the constraint flags and level_idc. The encoder
/// decides these (libva-direct writes High at level 4.2 or above by frame size
/// and rate, openh264 Main or Baseline), and a hardware decoder configured for
/// another profile may refuse the stream. `None` when the unit has no SPS.
pub fn codec_string_from_sps(au: &[u8]) -> Option<String> {
    let mut i = 0;
    while i + 6 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 && au[i + 3] & 0x1f == 7 {
            return Some(format!(
                "avc1.{:02x}{:02x}{:02x}",
                au[i + 4],
                au[i + 5],
                au[i + 6]
            ));
        }
        i += 1;
    }
    None
}

/// The warning `main.ts` printed once a decoder had answered 30 access units
/// in a row without a picture of their own.
pub fn not_output_warning(total: u64, streak: u64) -> Option<String> {
    (streak == 30).then(|| {
        format!(
            "H.264 decoder: no picture for 30 access units in a row ({total} in all); \
             output delay? every one is reported refused in the frame ack"
        )
    })
}

/// The browser's `VideoDecoder`, or a fake in the tests.
pub trait VideoCodec {
    /// Make a new decoder instance; its callbacks carry `generation`. The
    /// previous instance is closed or abandoned by then.
    fn open(&mut self, generation: u32);
    fn configure(&mut self, codec: &str, coded_size: (u32, u32)) -> Result<(), String>;
    /// Hand one chunk over; `timestamp` is its `frame_seq`. `Err` is the
    /// synchronous exception (`DataError`: a delta before any key chunk;
    /// `InvalidStateError`: closed).
    fn decode(&mut self, key: bool, timestamp: u32, data: &[u8]) -> Result<(), String>;
    fn is_closed(&self) -> bool;
    fn close(&mut self);
}

pub struct FrameDecoder<C: VideoCodec> {
    codec: C,
    generation: u32,
    /// Set by `close()`: the owner is done, never rebuild.
    disposed: bool,
    /// An async decode error closes the decoder for good, and the delta chunks
    /// that follow cannot be decoded without the reference they lost. Until
    /// the next key chunk, deltas are dropped; on it, a fresh decoder is built.
    awaiting_keyframe: bool,
    /// The frame_seq of the last chunk given to the decoder.
    last_frame_seq: u32,
    /// Chunks the decoder accepted and has not answered for, in decode order.
    /// An output for chunk N answers every earlier one too: WebCodecs outputs
    /// in decode order, so one that never came out never will.
    pending: VecDeque<u32>,
    /// The codec string the decoder is configured with.
    codec_string: String,
    /// The string in force before the SPS-derived one, to go back to once if
    /// the browser fails on the derived one before any picture came out.
    fallback: Option<String>,
    /// A derived string the browser refused: not tried again.
    refused: Option<String>,
    /// What the core must be told, in order: `(frame_seq, outcome)`.
    notes: Vec<(u32, H264Outcome)>,
    /// Console warnings for the shell to print.
    warnings: Vec<String>,
}

impl<C: VideoCodec> FrameDecoder<C> {
    /// Opens and configures the first decoder. An `Err` is a configure the
    /// browser threw on; the owner tries again with the next access unit, as
    /// `main.ts` did when `new FullFrameDecoder` threw.
    pub fn new(codec: C) -> Result<Self, String> {
        let mut d = FrameDecoder {
            codec,
            generation: 0,
            disposed: false,
            awaiting_keyframe: false,
            last_frame_seq: 0,
            pending: VecDeque::new(),
            codec_string: DEFAULT_CODEC.to_string(),
            fallback: None,
            refused: None,
            notes: Vec::new(),
            warnings: Vec::new(),
        };
        d.open()?;
        Ok(d)
    }

    fn open(&mut self) -> Result<(), String> {
        self.generation = self.generation.wrapping_add(1);
        self.codec.open(self.generation);
        self.codec.configure(&self.codec_string, CODED_SIZE)
    }

    /// The generation of the decoder in use.
    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn codec_string(&self) -> &str {
        &self.codec_string
    }

    pub fn codec(&self) -> &C {
        &self.codec
    }

    pub fn codec_mut(&mut self) -> &mut C {
        &mut self.codec
    }

    /// The decoder of `generation` output a picture for `timestamp`. `false`
    /// for a replaced decoder's output: the caller closes the frame and draws
    /// nothing. `true`: the caller hands the frame to the renderer.
    pub fn on_output(&mut self, generation: u32, timestamp: u32) -> bool {
        if generation != self.generation {
            return false;
        }
        self.fallback = None; // the configured string works
        self.answer(timestamp);
        true
    }

    /// The decoder of `generation` failed (its error callback).
    pub fn on_error(&mut self, generation: u32) {
        if generation != self.generation {
            return;
        }
        self.awaiting_keyframe = true;
        // A string derived from the SPS that the browser would not decode: the
        // rebuild on the next key chunk uses the one before it, once.
        if let Some(before) = self.fallback.take() {
            self.refused = Some(std::mem::replace(&mut self.codec_string, before));
        }
        // Everything in flight before the chunk that failed produced nothing;
        // the failed chunk itself is the failure.
        let last = self.last_frame_seq;
        self.answer_all(H264Outcome::NotOutput, Some(last));
        self.notes.push((last, H264Outcome::DecodeError));
    }

    fn answer(&mut self, timestamp: u32) {
        let Some(i) = self.pending.iter().position(|&s| s == timestamp) else {
            return;
        };
        let earlier: Vec<u32> = self.pending.drain(..i).collect();
        for seq in earlier {
            self.notes.push((seq, H264Outcome::NotOutput));
        }
        self.pending.pop_front();
        self.notes.push((timestamp, H264Outcome::Decoded));
    }

    fn answer_all(&mut self, outcome: H264Outcome, except: Option<u32>) {
        for seq in std::mem::take(&mut self.pending) {
            if Some(seq) != except {
                self.notes.push((seq, outcome));
            }
        }
    }

    /// One access unit. A key chunk to a healthy decoder is just decoded
    /// (WebCodecs takes an IDR without a reset; rebuilding would discard the P
    /// frames in flight). The decoder is rebuilt only after a failure, on the
    /// key chunk that follows it. `Err` is the browser's synchronous rejection,
    /// already reported to the core as a decode error.
    pub fn decode(&mut self, data: &[u8], key: bool, frame_seq: u32) -> Result<(), String> {
        if self.disposed {
            return Ok(());
        }
        if self.awaiting_keyframe || self.codec.is_closed() {
            if !key {
                self.notes.push((frame_seq, H264Outcome::NotOutput));
                return Ok(());
            }
            if !self.codec.is_closed() {
                self.codec.close();
            }
            self.answer_all(H264Outcome::NotOutput, None);
            self.awaiting_keyframe = false;
            self.open()?;
        }
        if key {
            // The IDR says what the stream is. A key chunk is the place to
            // change: a configure() makes the next chunk start from a key chunk.
            if let Some(derived) = codec_string_from_sps(data) {
                if derived != self.codec_string && self.refused.as_deref() != Some(derived.as_str())
                {
                    let before = std::mem::replace(&mut self.codec_string, derived);
                    match self.codec.configure(&self.codec_string, CODED_SIZE) {
                        Ok(()) => self.fallback = Some(before),
                        Err(e) => {
                            self.warnings.push(format!(
                                "H264 codec string {} refused; keeping {before} {e}",
                                self.codec_string
                            ));
                            self.refused = Some(std::mem::replace(&mut self.codec_string, before));
                            self.codec.configure(&self.codec_string, CODED_SIZE)?;
                        }
                    }
                }
            }
        }
        self.last_frame_seq = frame_seq;
        // Queued before the call, as decoder.ts did.
        self.pending.push_back(frame_seq);
        if let Err(e) = self.codec.decode(key, frame_seq, data) {
            // DataError: a delta while the decoder still wants a key chunk
            // (lost session-start IDR, or a key chunk the browser rejected).
            // The deltas that follow are dropped quietly.
            self.awaiting_keyframe = true;
            self.pending.pop_back();
            self.answer_all(H264Outcome::NotOutput, None);
            self.notes.push((frame_seq, H264Outcome::DecodeError));
            return Err(e);
        }
        Ok(())
    }

    /// What the core must hear (`ClientCore::h264_result`), oldest first.
    pub fn take_notes(&mut self) -> Vec<(u32, H264Outcome)> {
        std::mem::take(&mut self.notes)
    }

    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    pub fn close(&mut self) {
        self.disposed = true;
        if !self.codec.is_closed() {
            self.codec.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    struct FakeLog {
        opens: u32,
        configured: Vec<String>,
        decoded: Vec<(u32, bool)>,
        closed: bool,
        need_key: bool,
        refuse_codec: Option<String>,
        reject_next_decode: bool,
    }

    struct FakeCodec(Rc<RefCell<FakeLog>>);

    impl VideoCodec for FakeCodec {
        fn open(&mut self, _generation: u32) {
            let mut l = self.0.borrow_mut();
            l.opens += 1;
            l.closed = false;
            l.need_key = true;
        }
        fn configure(&mut self, codec: &str, _size: (u32, u32)) -> Result<(), String> {
            let mut l = self.0.borrow_mut();
            if l.refuse_codec.as_deref() == Some(codec) {
                return Err("unsupported".into());
            }
            l.configured.push(codec.to_string());
            Ok(())
        }
        fn decode(&mut self, key: bool, timestamp: u32, _data: &[u8]) -> Result<(), String> {
            let mut l = self.0.borrow_mut();
            if l.closed {
                return Err("InvalidStateError: closed".into());
            }
            if (l.need_key && !key) || l.reject_next_decode {
                l.reject_next_decode = false;
                return Err("DataError: key frame required".into());
            }
            l.need_key = false;
            l.decoded.push((timestamp, key));
            Ok(())
        }
        fn is_closed(&self) -> bool {
            self.0.borrow().closed
        }
        fn close(&mut self) {
            self.0.borrow_mut().closed = true;
        }
    }

    use H264Outcome::{DecodeError, Decoded, NotOutput};

    fn harness() -> (FrameDecoder<FakeCodec>, Rc<RefCell<FakeLog>>) {
        let log = Rc::new(RefCell::new(FakeLog::default()));
        let d = FrameDecoder::new(FakeCodec(log.clone())).unwrap();
        (d, log)
    }

    fn emit(d: &mut FrameDecoder<FakeCodec>, ts: u32) {
        let g = d.generation();
        assert!(d.on_output(g, ts));
    }

    const NAL: &[u8] = &[0];

    fn sps(p: u8, c: u8, l: u8) -> Vec<u8> {
        vec![0, 0, 0, 1, 0x67, p, c, l, 0xe9, 0x00]
    }
    const IDR: &[u8] = &[0, 0, 1, 0x65, 0x88, 0x84];
    fn with_idr(mut v: Vec<u8>) -> Vec<u8> {
        v.extend_from_slice(IDR);
        v
    }

    /// h264_frame_seq: tags each output with the seq of its own chunk after a rejected chunk.
    #[test]
    fn tags_each_output_with_its_own_seq_after_a_rejected_chunk() {
        let (mut d, log) = harness();
        assert!(d.decode(NAL, false, 10).is_err());
        d.decode(NAL, true, 11).unwrap();
        emit(&mut d, 11);
        d.decode(NAL, false, 12).unwrap();
        emit(&mut d, 12);
        assert_eq!(
            d.take_notes(),
            vec![(10, DecodeError), (11, Decoded), (12, Decoded)]
        );
        assert_eq!(log.borrow().opens, 2);
    }

    /// h264_frame_seq: rebuilds after an async decode error, on the next key chunk.
    #[test]
    fn rebuilds_after_an_async_error_on_the_next_key_chunk() {
        let (mut d, log) = harness();
        d.decode(NAL, true, 1).unwrap();
        emit(&mut d, 1);
        log.borrow_mut().closed = true;
        let g = d.generation();
        d.on_error(g);
        d.take_notes();
        assert!(d.decode(NAL, false, 2).is_ok());
        assert_eq!(d.take_notes(), vec![(2, NotOutput)]);
        assert_eq!(log.borrow().opens, 1);
        d.decode(NAL, true, 3).unwrap();
        assert_eq!(log.borrow().opens, 2);
        d.decode(NAL, false, 4).unwrap();
        emit(&mut d, 3);
        emit(&mut d, 4);
        assert_eq!(d.take_notes(), vec![(3, Decoded), (4, Decoded)]);
    }

    /// h264_frame_seq: does not rebuild after close().
    #[test]
    fn does_not_rebuild_after_close() {
        let (mut d, log) = harness();
        d.close();
        d.decode(NAL, true, 1).unwrap();
        assert_eq!(log.borrow().opens, 1);
        assert!(d.take_notes().is_empty());
    }

    /// h264_frame_seq: a late error from a replaced decoder does not poison the new one.
    #[test]
    fn a_late_error_from_a_replaced_decoder_is_ignored() {
        let (mut d, log) = harness();
        log.borrow_mut().closed = true;
        let g1 = d.generation();
        d.on_error(g1);
        d.take_notes();
        d.decode(NAL, true, 5).unwrap();
        assert_eq!(d.generation(), 2);
        d.on_error(g1);
        assert!(d.take_notes().is_empty());
        d.decode(NAL, false, 6).unwrap();
        emit(&mut d, 5);
        emit(&mut d, 6);
        assert_eq!(d.take_notes(), vec![(5, Decoded), (6, Decoded)]);
    }

    /// h264_frame_seq: after a rejected delta-before-key, later deltas drop quietly.
    #[test]
    fn after_a_rejected_delta_before_key_later_deltas_drop_quietly() {
        let (mut d, _log) = harness();
        assert!(d.decode(NAL, false, 1).is_err());
        assert!(d.decode(NAL, false, 2).is_ok());
        d.decode(NAL, true, 3).unwrap();
        d.decode(NAL, false, 4).unwrap();
        emit(&mut d, 3);
        emit(&mut d, 4);
        assert_eq!(
            d.take_notes(),
            vec![(1, DecodeError), (2, NotOutput), (3, Decoded), (4, Decoded)]
        );
    }

    /// h264_result: decoded means the output fired for its own timestamp.
    #[test]
    fn decoded_means_the_output_fired_for_its_own_timestamp() {
        let (mut d, _log) = harness();
        d.decode(NAL, true, 10).unwrap();
        d.decode(NAL, false, 11).unwrap();
        assert!(d.take_notes().is_empty());
        emit(&mut d, 10);
        assert_eq!(d.take_notes(), vec![(10, Decoded)]);
        emit(&mut d, 11);
        assert_eq!(d.take_notes(), vec![(11, Decoded)]);
    }

    /// h264_result: an output overtakes the chunks before it.
    #[test]
    fn an_output_overtakes_the_chunks_before_it() {
        let (mut d, _log) = harness();
        d.decode(NAL, true, 10).unwrap();
        d.decode(NAL, false, 11).unwrap();
        d.decode(NAL, false, 12).unwrap();
        emit(&mut d, 12);
        assert_eq!(
            d.take_notes(),
            vec![(10, NotOutput), (11, NotOutput), (12, Decoded)]
        );
    }

    /// h264_result: an async error answers the chunks in flight and reports the last.
    #[test]
    fn an_async_error_answers_in_flight_chunks_and_reports_the_last() {
        let (mut d, log) = harness();
        d.decode(NAL, true, 1).unwrap();
        d.decode(NAL, false, 2).unwrap();
        d.decode(NAL, false, 3).unwrap();
        log.borrow_mut().closed = true;
        let g = d.generation();
        d.on_error(g);
        assert_eq!(
            d.take_notes(),
            vec![(1, NotOutput), (2, NotOutput), (3, DecodeError)]
        );
        d.decode(NAL, false, 4).unwrap();
        assert_eq!(d.take_notes(), vec![(4, NotOutput)]);
    }

    /// h264_result: a chunk the browser rejects is a failure, not a pending frame.
    #[test]
    fn a_chunk_the_browser_rejects_is_a_failure_not_a_pending_frame() {
        let (mut d, log) = harness();
        d.decode(NAL, true, 1).unwrap();
        log.borrow_mut().reject_next_decode = true;
        assert!(d.decode(NAL, false, 2).is_err());
        assert_eq!(d.take_notes(), vec![(1, NotOutput), (2, DecodeError)]);
    }

    /// h264_result: an IDR to a healthy decoder neither rebuilds nor discards.
    #[test]
    fn an_idr_to_a_healthy_decoder_neither_rebuilds_nor_discards() {
        let (mut d, log) = harness();
        d.decode(NAL, true, 1).unwrap();
        d.decode(NAL, false, 2).unwrap();
        d.decode(NAL, true, 9).unwrap();
        assert_eq!(log.borrow().opens, 1);
        assert!(d.take_notes().is_empty());
        emit(&mut d, 2);
        emit(&mut d, 9);
        assert_eq!(
            d.take_notes(),
            vec![(1, NotOutput), (2, Decoded), (9, Decoded)]
        );
    }

    /// h264_result: after a failure the next key chunk rebuilds the decoder.
    #[test]
    fn after_a_failure_the_next_key_chunk_rebuilds() {
        let (mut d, log) = harness();
        d.decode(NAL, true, 1).unwrap();
        log.borrow_mut().closed = true;
        let g = d.generation();
        d.on_error(g);
        d.take_notes();
        d.decode(NAL, true, 9).unwrap();
        assert_eq!(log.borrow().opens, 2);
        emit(&mut d, 9);
        assert_eq!(d.take_notes().last(), Some(&(9, Decoded)));
    }

    /// h264_result: reads profile, constraints and level from the SPS.
    #[test]
    fn the_sps_gives_profile_constraints_and_level() {
        assert_eq!(
            codec_string_from_sps(&sps(0x64, 0, 0x2a)).as_deref(),
            Some("avc1.64002a")
        );
        assert_eq!(
            codec_string_from_sps(&sps(0x4d, 0x40, 0x1e)).as_deref(),
            Some("avc1.4d401e")
        );
        assert_eq!(
            codec_string_from_sps(&with_idr(sps(0x64, 0, 0x2a))).as_deref(),
            Some("avc1.64002a")
        );
    }

    /// h264_result: finds nothing in a delta frame or a truncated SPS.
    #[test]
    fn no_codec_string_in_a_delta_or_a_truncated_sps() {
        assert_eq!(codec_string_from_sps(&[0, 0, 1, 0x41, 1, 2, 3, 4]), None);
        assert_eq!(codec_string_from_sps(&[0, 0, 0, 1, 0x67, 0x64]), None);
        assert_eq!(codec_string_from_sps(&[]), None);
    }

    /// h264_result: starts as High 4.2 and follows the stream.
    #[test]
    fn starts_as_high_4_2_and_follows_the_stream() {
        let (mut d, log) = harness();
        assert_eq!(log.borrow().configured, ["avc1.64002a"]);
        d.decode(&with_idr(sps(0x64, 0, 0x2a)), true, 1).unwrap();
        assert_eq!(log.borrow().configured, ["avc1.64002a"]);
        d.decode(&with_idr(sps(0x4d, 0x40, 0x1f)), true, 2).unwrap();
        d.decode(&with_idr(sps(0x4d, 0x40, 0x1f)), true, 3).unwrap();
        assert_eq!(log.borrow().configured, ["avc1.64002a", "avc1.4d401f"]);
        d.decode(&[0, 0, 1, 0x41, 1, 2], false, 4).unwrap();
        assert_eq!(log.borrow().configured, ["avc1.64002a", "avc1.4d401f"]);
    }

    /// h264_result: keeps the previous string when configure refuses the derived one.
    #[test]
    fn keeps_the_previous_string_when_configure_refuses() {
        let (mut d, log) = harness();
        log.borrow_mut().refuse_codec = Some("avc1.4d401f".into());
        d.decode(&with_idr(sps(0x4d, 0x40, 0x1f)), true, 1).unwrap();
        assert_eq!(log.borrow().configured, ["avc1.64002a", "avc1.64002a"]);
        assert_eq!(d.take_warnings().len(), 1);
        d.decode(&with_idr(sps(0x4d, 0x40, 0x1f)), true, 2).unwrap();
        assert!(!log.borrow().configured.iter().any(|c| c == "avc1.4d401f"));
        assert_eq!(log.borrow().configured.len(), 2);
    }

    /// h264_result: goes back once when the derived string fails before any picture.
    #[test]
    fn goes_back_once_when_the_derived_string_fails_before_any_picture() {
        let (mut d, log) = harness();
        d.decode(&with_idr(sps(0x4d, 0x40, 0x1f)), true, 1).unwrap();
        assert_eq!(log.borrow().configured, ["avc1.64002a", "avc1.4d401f"]);
        let g = d.generation();
        d.on_error(g);
        d.decode(&with_idr(sps(0x4d, 0x40, 0x1f)), true, 2).unwrap();
        assert_eq!(
            log.borrow().configured.last().map(String::as_str),
            Some("avc1.64002a")
        );
        assert_eq!(log.borrow().opens, 2);
        assert_eq!(d.codec_string(), "avc1.64002a");
    }

    #[test]
    fn the_not_output_warning_fires_once_at_30() {
        assert!(not_output_warning(40, 29).is_none());
        assert!(not_output_warning(40, 30).is_some());
        assert!(not_output_warning(41, 31).is_none());
    }
}
