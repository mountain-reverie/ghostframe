//! B7-0: the stream parameters of a real openh264 stream, and the decoder
//! model checked against libavcodec on it.
//!
//! The fixture is 70 frames from libopenh264 2.6.0 (camera usage, long-term
//! references on, mark period 8, marking feedback, two LTR recoveries), made
//! by `tools/openh264-fixture/make.sh`. It is committed, so these tests need
//! neither the library nor a GPU.

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

use super::pcm_decoder::{PcmDecoder, Strictness};
use super::syntax::{
    parse_pps, parse_slice_header, parse_sps, split_annexb, BitWriter, DecoderModel, Mmco, RefPic,
    SliceHeader, StreamParams, NAL_IDR, NAL_PPS, NAL_SLICE, NAL_SPS,
};

const STREAM: &[u8] = include_bytes!("testdata/openh264_2_6_camera_ltr_128x128.h264");
const HASHES: &str = include_str!("testdata/openh264_2_6_camera_ltr_128x128.sha256");

/// The stream as access units: every slice NAL unit ends one, parameter sets
/// ahead of it belong to it. Each unit is Annex-B with 4-byte start codes.
fn access_units() -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    for nal in split_annexb(STREAM) {
        cur.extend_from_slice(&[0, 0, 0, 1]);
        cur.extend_from_slice(nal);
        if matches!(nal[0] & 0x1f, NAL_SLICE | NAL_IDR) {
            out.push(std::mem::take(&mut cur));
        }
    }
    assert!(cur.is_empty(), "trailing NAL units after the last slice");
    out
}

fn slice_nal(au: &[u8]) -> Vec<u8> {
    split_annexb(au)
        .into_iter()
        .find(|n| matches!(n[0] & 0x1f, NAL_SLICE | NAL_IDR))
        .expect("a slice in every access unit")
        .to_vec()
}

fn params() -> StreamParams {
    StreamParams::from_annexb(&access_units()[0]).expect("the IDR access unit carries SPS and PPS")
}

fn headers() -> Vec<SliceHeader> {
    let p = params();
    access_units()
        .iter()
        .map(|au| {
            parse_slice_header(&p, &slice_nal(au))
                .expect("slice header")
                .0
        })
        .collect()
}

#[test]
fn sps_and_pps_of_a_real_openh264_stream() {
    let au = &access_units()[0];
    let nals = split_annexb(au);
    let find = |t: u8| nals.iter().find(|n| n[0] & 0x1f == t).unwrap();
    let sps = parse_sps(find(NAL_SPS)).unwrap();
    assert_eq!(sps.profile_idc, 100, "High");
    assert_eq!(sps.log2_max_frame_num, 15);
    assert_eq!(sps.poc_type, 2);
    assert_eq!(sps.max_num_ref_frames, 3);
    assert!(sps.gaps_in_frame_num_allowed);
    assert_eq!((sps.width, sps.height), (128, 128));
    let pps = parse_pps(find(NAL_PPS)).unwrap();
    assert!(pps.cabac, "entropy_coding_mode_flag");
    assert_eq!(pps.sps_id, sps.sps_id);
    let p = params();
    assert_eq!(
        (
            p.log2_max_frame_num,
            p.poc_type,
            p.max_num_ref_frames,
            p.num_ref_idx_l0_default
        ),
        (15, 2, 3, 1)
    );
    assert!(p.gaps_in_frame_num_allowed && p.cabac);
    // The IDR is the only access unit that carries parameter sets.
    assert_eq!(
        access_units()
            .iter()
            .filter(|a| split_annexb(a).len() > 1)
            .count(),
        1
    );
}

#[test]
fn a_pps_for_another_sps_is_refused() {
    let au = &access_units()[0];
    let nals = split_annexb(au);
    let sps = nals.iter().find(|n| n[0] & 0x1f == NAL_SPS).unwrap();
    let mut w = BitWriter::default();
    w.put_ue(0); // pic_parameter_set_id
    w.put_ue(1); // seq_parameter_set_id: not the SPS's
    w.put_bit(true); // CABAC
    w.put_bit(false);
    w.put_ue(0);
    w.put_ue(0);
    w.put_ue(0);
    w.put_bit(false);
    w.put_bits(0, 2);
    for _ in 0..3 {
        w.put_se(0);
    }
    w.put_bit(true); // deblocking_filter_control_present_flag
    w.put_bit(false);
    w.put_bit(false);
    w.put_bit(true); // rbsp_stop_one_bit
    let mut pps = vec![0x68];
    pps.extend(w.into_bytes());
    assert_eq!(
        parse_pps(&pps).unwrap().sps_id,
        1,
        "the hand-made PPS parses"
    );
    assert!(StreamParams::from_sps_pps(sps, &pps).is_err());
}

#[test]
fn every_slice_header_parses_and_the_marks_are_there() {
    let hs = headers();
    assert!(hs.len() >= 70, "{} slices", hs.len());
    assert!(
        hs[0].idr && hs[1..].iter().all(|h| !h.idr),
        "one IDR, first"
    );
    let mmcos: Vec<&Vec<Mmco>> = hs.iter().filter_map(|h| h.mmco.as_ref()).collect();
    let mmco6 = mmcos
        .iter()
        .filter(|m| m.iter().any(|o| matches!(o, Mmco::CurrentToLong(_))))
        .count();
    let mmco3 = mmcos
        .iter()
        .filter(|m| m.iter().any(|o| matches!(o, Mmco::ShortToLong(..))))
        .count();
    assert!(mmco6 >= 1, "premise: a picture marked by MMCO 6");
    assert!(
        mmco3 >= 4,
        "premise: at least four marks by MMCO 3, got {mmco3}"
    );
    assert!(hs.iter().all(|h| h.nal_ref_idc == 3));
    assert!(
        hs.iter().skip(1).all(|h| !h.rplm.is_empty()),
        "openh264 writes an explicit list modification on every P"
    );
    // Frame numbers count 0.. without a gap.
    assert!(hs
        .iter()
        .enumerate()
        .all(|(i, h)| h.frame_num as usize == i));
}

/// POC type 2: output order is decode order, `2 * FrameNumOffset+frame_num`,
/// a non-reference picture one less; `frame_num` wrap adds `MaxFrameNum`.
#[test]
fn poc_type_2_follows_frame_num_across_the_wrap() {
    let p = StreamParams {
        log2_max_frame_num: 4,
        poc_type: 2,
        ..StreamParams::emitted()
    };
    let mut m = DecoderModel::new(p);
    let idr = m.push(0, &SliceHeader::idr(0, false));
    assert_eq!(idr.poc, 0);
    let mut last = 0;
    for (tag, fnum) in (1..40u32).map(|t| (t, t % 16)) {
        let e = m.push(tag, &SliceHeader::p(fnum, 0, 3));
        assert_eq!(e.poc, 2 * i64::from(tag), "frame {tag} (frame_num {fnum})");
        assert!(e.poc > last);
        last = e.poc;
    }
    let e = m.push(40, &SliceHeader::p(40 % 16, 0, 0));
    assert_eq!(e.poc, 2 * 40 - 1, "a non-reference picture");
}

#[test]
#[should_panic(expected = "POC type 0 only")]
fn the_writer_refuses_poc_type_2() {
    let p = StreamParams {
        poc_type: 2,
        ..StreamParams::writer()
    };
    let _ = super::syntax::write_slice_header(&p, &SliceHeader::p(1, 0, 3));
}

#[test]
fn the_model_replays_the_stream_without_a_violation() {
    let mut m = DecoderModel::new(params());
    for (i, h) in headers().iter().enumerate() {
        let e = m.push(i as u32, h);
        assert!(!e.violations.any(), "frame {i}: {:?}", e.violations);
        assert_eq!(e.poc, 2 * i as i64);
        assert!(!e.list0.contains(&RefPic::Missing), "frame {i}");
    }
}

// ------------------------------------------------------- the decode oracle

fn sha(yuv: &[u8]) -> String {
    Sha256::digest(yuv)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Decode the stream without the frames in `skip`. `None` where no picture
/// came out of the call.
fn decode(strictness: Strictness, skip: &[usize]) -> Vec<Option<Vec<u8>>> {
    let mut d = PcmDecoder::new(strictness);
    access_units()
        .iter()
        .enumerate()
        .filter(|(i, _)| !skip.contains(i))
        .map(|(i, au)| {
            let _ = i;
            d.feed(au, i as i64)
                .ok()
                .filter(|p| !p.damaged)
                .map(|p| p.yuv)
        })
        .collect()
}

#[test]
fn the_full_decode_matches_the_committed_hashes() {
    let full = decode(Strictness::Strict, &[]);
    let want: Vec<&str> = HASHES.lines().collect();
    assert_eq!(full.len(), want.len());
    for (i, (got, want)) in full.iter().zip(&want).enumerate() {
        let got = got
            .as_ref()
            .unwrap_or_else(|| panic!("frame {i}: no picture"));
        assert_eq!(sha(got), *want, "frame {i}");
    }
}

/// What the model says each frame depends on: the pictures it predicts from,
/// transitively, plus the carrier of the marking that made a long-term
/// reference one (without that picture the slot is not there).
struct Ancestry {
    deps: Vec<BTreeSet<usize>>,
    /// Pictures that were ever long-term, with the frame that marked them.
    marked_by: BTreeMap<usize, usize>,
    /// Frames whose reference was a long-term picture that was not the
    /// previous frame: `(frame, reference)`.
    lt_refs: Vec<(usize, usize)>,
}

fn ancestry() -> Ancestry {
    let hs = headers();
    let mut m = DecoderModel::new(params());
    let mut deps: Vec<BTreeSet<usize>> = Vec::new();
    let mut marked_by = BTreeMap::new();
    let mut lt_refs = Vec::new();
    for (f, h) in hs.iter().enumerate() {
        let before: Vec<Option<RefPic>> = (0..4).map(|i| m.long_term(i)).collect();
        let e = m.push(f as u32, h);
        let mut d = BTreeSet::new();
        if let Some(RefPic::Real(r)) = e.reference {
            let r = r as usize;
            d.insert(r);
            d.extend(deps[r].iter().copied());
            if before.contains(&Some(RefPic::Real(r as u32))) {
                // Held long-term: its slot exists only because of its carrier.
                d.insert(marked_by[&r]);
                if r + 1 != f {
                    lt_refs.push((f, r));
                }
            }
        }
        deps.push(d);
        for i in 0..4 {
            if let Some(RefPic::Real(p)) = m.long_term(i) {
                if before[i as usize] != Some(RefPic::Real(p)) {
                    marked_by.insert(p as usize, f);
                }
            }
        }
    }
    Ancestry {
        deps,
        marked_by,
        lt_refs,
    }
}

/// Drop each frame in turn, decode the rest with libavcodec, and compare with
/// the full decode: the frames whose picture changes must be exactly the ones
/// the model says depend on the dropped one.
#[test]
fn the_model_names_the_frames_a_lost_frame_corrupts() {
    let full: Vec<Vec<u8>> = decode(Strictness::Strict, &[])
        .into_iter()
        .map(|p| p.expect("full decode"))
        .collect();
    let distinct: BTreeSet<String> = full.iter().map(|p| sha(p)).collect();
    assert_eq!(distinct.len(), full.len(), "premise: every picture differs");

    let a = ancestry();
    let n = full.len();
    let mut differs_of: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
    let mut mismatches = Vec::new();
    for g in 1..n {
        let got = decode(Strictness::Lenient, &[g]);
        // `got[k]` is the picture of frame k for k < g, of frame k + 1 after.
        let mut differs = BTreeSet::new();
        for f in g + 1..n {
            match &got[f - 1] {
                Some(p) if *p == full[f] => {}
                _ => {
                    differs.insert(f);
                }
            }
        }
        let expected: BTreeSet<usize> = (g + 1..n).filter(|&f| a.deps[f].contains(&g)).collect();
        if differs != expected {
            mismatches.push(format!(
                "drop {g}: libavcodec {:?} model {:?}",
                differs, expected
            ));
        }
        differs_of.insert(g, differs);
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));

    eprintln!(
        "oracle: {} drops, {} change later pictures, {} only some of them; recoveries (frame, LT picture, marker) {:?}",
        differs_of.len(),
        differs_of.values().filter(|d| !d.is_empty()).count(),
        differs_of
            .iter()
            .filter(|(g, d)| !d.is_empty() && d.len() < n - 1 - **g)
            .count(),
        a.lt_refs
            .iter()
            .map(|&(f, r)| (f, r, a.marked_by[&r]))
            .collect::<Vec<_>>()
    );

    // Premises: the oracle can fail.
    assert!(
        differs_of.values().any(|d| !d.is_empty()),
        "premise: some drop changes later pictures"
    );
    let strict = differs_of
        .iter()
        .filter(|(g, d)| !d.is_empty() && d.len() < n - 1 - **g)
        .count();
    assert!(
        strict >= 1,
        "premise: a drop whose later frames are not all affected"
    );
    assert!(
        a.marked_by
            .iter()
            .any(|(&p, _)| p != 0 && !differs_of[&p].is_empty()),
        "premise: a long-term-held picture whose loss matters"
    );
    // Both kinds of recovery P, each skipping frames.
    let mut kinds = BTreeSet::new();
    for &(f, r) in &a.lt_refs {
        let by_mmco6 = a.marked_by[&r] == r;
        kinds.insert(by_mmco6);
        // The carrier of the mark is the one frame in between the recovery
        // does need (it made the picture long-term).
        let skipped: Vec<usize> = (r + 1..f).filter(|&g| g != a.marked_by[&r]).collect();
        assert!(!skipped.is_empty(), "recovery {f} from {r} skips nothing");
        for g in skipped {
            assert!(
                !differs_of[&g].contains(&f),
                "recovery P {f} (from {r}) must not depend on skipped frame {g}"
            );
        }
    }
    assert_eq!(
        kinds,
        BTreeSet::from([true, false]),
        "premise: recoveries from an MMCO 6 LT and from an MMCO 3 LT; got {:?}",
        a.lt_refs
    );
}
