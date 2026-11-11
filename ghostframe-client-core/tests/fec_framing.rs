//! The rev 6 framing: datagrams arrive as rev 6 FEC frames, are
//! unbundled by `FecRx` and the records go through the ordinary tile path.

use ghostframe_client_core::{ClientConfig, ClientCore, Event, PollOutput};
use ghostframe_protocol::fec::codec::encode_repairs;
use ghostframe_protocol::fec::frame::{push_record, RepairFrame, SourceFrame};
use ghostframe_protocol::fec::repair_status::{
    StatusReader, StatusRecord, FEC_REPAIR_STATUS_STREAM_TAG,
};
use ghostframe_protocol::protocol::{
    fragment_frame, fragment_tile, Codec, TileFragmentInputs, TILE_DATAGRAM_FLAG,
};

fn core() -> ClientCore {
    let mut c = ClientCore::new(
        ClientConfig {
            indices_raw_enabled: true,
            ..Default::default()
        },
        0,
    );
    while c.poll_transmit(0).is_some() {}
    c
}

fn tile(frame_seq: u32, x: u8, y: u8, codec: Codec, payload: &[u8]) -> Vec<u8> {
    let mut v = fragment_tile(
        &TileFragmentInputs {
            frame_seq: frame_seq | TILE_DATAGRAM_FLAG,
            tile_x: x,
            tile_y: y,
            codec,
            generation: 1,
            pass: 0,
            timestamp_us: 0,
        },
        payload,
        1200,
    );
    assert_eq!(v.len(), 1);
    v.remove(0)
}

fn solid(x: u8, y: u8) -> Vec<u8> {
    tile(1, x, y, Codec::Solid, &[0x11, 0x22, 0x33, 0xFF])
}

fn records(items: &[&[u8]]) -> Vec<u8> {
    let mut v = Vec::new();
    for i in items {
        push_record(&mut v, i);
    }
    v
}

fn source(seq: u32, block: u16, idx: u8, last: bool, recs: &[u8]) -> SourceFrame<'_> {
    SourceFrame {
        last,
        bundle_seq: seq,
        block_id: block,
        idx,
        records: recs,
    }
}

/// Every output the core has queued.
fn drain(c: &mut ClientCore, now: u64) -> Vec<PollOutput> {
    let mut v = Vec::new();
    while let Some(o) = c.poll_transmit(now) {
        v.push(o);
    }
    v
}

fn only_tiles(evs: &[Event]) -> Vec<&Event> {
    evs.iter()
        .filter(|e| matches!(e, Event::TileReady { .. }))
        .collect()
}

#[test]
fn framed_tiles_are_delivered_and_ack_v4_only() {
    // A solid tile and a second solid tile at another position.
    let a = solid(3, 4);
    let b = solid(5, 6);

    let mut framed = core();
    let recs = records(&[&a, &b]);
    let dg = source(10, 1, 0, true, &recs).encode();
    let got = framed.handle_datagram(&dg, 1_000);
    assert_eq!(only_tiles(&got).len(), 2);

    // Let the ACK batcher fire.
    let mut outs = drain(&mut framed, 1_000);
    let mut now = 1_000;
    for _ in 0..5 {
        let Some(t) = framed.poll_timeout() else {
            break;
        };
        now = t.max(now);
        framed.on_timeout(now);
        outs.extend(drain(&mut framed, now));
        if outs
            .iter()
            .any(|o| matches!(o, PollOutput::Datagram(d) if d[0] == 0x08))
        {
            break;
        }
    }
    let types: Vec<u8> = outs
        .iter()
        .filter_map(|o| match o {
            PollOutput::Datagram(d) => Some(d[0]),
            _ => None,
        })
        .collect();
    assert!(
        !types.contains(&0x06),
        "no v3 ACK batch under framing: {types:?}"
    );
    assert_eq!(
        types.iter().filter(|&&t| t == 0x08).count(),
        1,
        "exactly one ACK v4: {types:?}"
    );
}

#[test]
fn rebuilt_tile_is_delivered_and_recovered_is_reported() {
    let d: Vec<Vec<u8>> = (0..3).map(|i| solid(i, 0)).collect();
    let recs: Vec<Vec<u8>> = d.iter().map(|x| records(&[x])).collect();
    let srcs: Vec<SourceFrame> = recs
        .iter()
        .enumerate()
        .map(|(i, r)| source(20 + i as u32, 7, i as u8, i == 2, r))
        .collect();
    let syms: Vec<Vec<u8>> = srcs.iter().map(|s| s.coded_symbol()).collect();
    let refs: Vec<&[u8]> = syms.iter().map(|s| s.as_slice()).collect();
    let rep = encode_repairs(&refs, &[0]).unwrap();
    let repair = RepairFrame {
        bundle_seq: 99,
        block_id: 7,
        k: 3,
        r: 1,
        repair_idx: 0,
        payload: &rep[0],
    }
    .encode();

    let mut c = core();
    let mut evs = c.handle_datagram(&srcs[0].encode(), 1_000);
    evs.extend(c.handle_datagram(&srcs[2].encode(), 1_001));
    evs.extend(c.handle_datagram(&repair, 1_002));
    let xs: Vec<u8> = evs
        .iter()
        .filter_map(|e| match e {
            Event::TileReady { tile_x, .. } => Some(*tile_x),
            _ => None,
        })
        .collect();
    let mut sorted = xs.clone();
    sorted.sort();
    assert_eq!(sorted, vec![0, 1, 2], "all three tiles, got {xs:?}");

    let status: Vec<u8> = drain(&mut c, 1_002)
        .into_iter()
        .filter_map(|o| match o {
            PollOutput::RepairStatus(b) => Some(b),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(status[0], FEC_REPAIR_STATUS_STREAM_TAG);
    let recs = StatusReader::default().push(&status[1..]).unwrap();
    assert_eq!(recs, vec![StatusRecord::Recovered(vec![21])]);
}

#[test]
fn rejected_cdf53_pass_reports_content_rejected() {
    let good = solid(0, 0);
    // Truncated CDF 5/3 pass: fails prevalidation.
    let bad = tile(2, 4, 5, Codec::Cdf53, &[0x00, 0x01, 0xFF]);
    let recs = records(&[&good, &bad]);
    let mut c = core();
    let evs = c.handle_datagram(&source(33, 2, 0, true, &recs).encode(), 1_000);
    assert!(evs.iter().any(|e| matches!(e, Event::DecodeError { .. })));
    let status: Vec<u8> = drain(&mut c, 1_000)
        .into_iter()
        .filter_map(|o| match o {
            PollOutput::RepairStatus(b) => Some(b),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(status[0], FEC_REPAIR_STATUS_STREAM_TAG);
    let got = StatusReader::default().push(&status[1..]).unwrap();
    assert_eq!(got, vec![StatusRecord::ContentRejected(vec![(33, 1)])]);
}

/// Status records the core queued, decoded.
fn status_records(c: &mut ClientCore, now: u64) -> Vec<StatusRecord> {
    let status: Vec<u8> = drain(c, now)
        .into_iter()
        .filter_map(|o| match o {
            PollOutput::RepairStatus(b) => Some(b),
            _ => None,
        })
        .flatten()
        .collect();
    if status.is_empty() {
        return Vec::new();
    }
    assert_eq!(status[0], FEC_REPAIR_STATUS_STREAM_TAG);
    StatusReader::default().push(&status[1..]).unwrap()
}

/// Datagram type bytes after letting the ACK batcher fire.
fn ack_types(c: &mut ClientCore, from: u64) -> Vec<u8> {
    let mut outs = drain(c, from);
    let mut now = from;
    for _ in 0..5 {
        let Some(t) = c.poll_timeout() else { break };
        now = t.max(now);
        c.on_timeout(now);
        outs.extend(drain(c, now));
        if outs
            .iter()
            .any(|o| matches!(o, PollOutput::Datagram(d) if d[0] == 0x08))
        {
            break;
        }
    }
    outs.iter()
        .filter_map(|o| match o {
            PollOutput::Datagram(d) => Some(d[0]),
            _ => None,
        })
        .collect()
}

fn tile_pass(frame_seq: u32, x: u8, y: u8, pass: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = fragment_tile(
        &TileFragmentInputs {
            frame_seq: frame_seq | TILE_DATAGRAM_FLAG,
            tile_x: x,
            tile_y: y,
            codec: Codec::Cdf53,
            generation: 1,
            pass,
            timestamp_us: 0,
        },
        payload,
        1200,
    );
    v.remove(0)
}

#[test]
fn rebuilt_bundle_with_a_bad_pass_rejects_before_it_recovers() {
    let good = records(&[&solid(0, 0)]);
    let bad = records(&[&tile(2, 4, 5, Codec::Cdf53, &[0x00, 0x01, 0xFF])]);
    let srcs = [source(40, 9, 0, false, &good), source(41, 9, 1, true, &bad)];
    let syms: Vec<Vec<u8>> = srcs.iter().map(|s| s.coded_symbol()).collect();
    let refs: Vec<&[u8]> = syms.iter().map(|s| s.as_slice()).collect();
    let rep = encode_repairs(&refs, &[0]).unwrap();
    let repair = RepairFrame {
        bundle_seq: 99,
        block_id: 9,
        k: 2,
        r: 1,
        repair_idx: 0,
        payload: &rep[0],
    }
    .encode();
    let mut c = core();
    c.handle_datagram(&srcs[0].encode(), 1_000);
    c.handle_datagram(&repair, 1_001);
    assert_eq!(
        status_records(&mut c, 1_001),
        vec![
            StatusRecord::ContentRejected(vec![(41, 0)]),
            StatusRecord::Recovered(vec![41])
        ]
    );
}

#[test]
fn out_of_grid_tile_in_a_source_is_content_rejected() {
    let mut dims = Vec::new();
    dims.extend_from_slice(&64u32.to_be_bytes());
    dims.extend_from_slice(&64u32.to_be_bytes());
    let sentinel = tile(1, 0xFF, 0xFF, Codec::Solid, &dims);
    let outside = solid(200, 200);
    let recs = records(&[&sentinel, &outside]);
    let mut c = core();
    let evs = c.handle_datagram(&source(50, 3, 0, true, &recs).encode(), 1_000);
    assert!(only_tiles(&evs).is_empty());
    assert_eq!(
        status_records(&mut c, 1_000),
        vec![StatusRecord::ContentRejected(vec![(50, 1)])]
    );
}

#[test]
fn h264_fragment_under_framing_is_acked_once_in_v4_only() {
    let mut dg = fragment_frame(4, 0, true, &[7u8; 100], 1200).remove(0);
    dg[8..12].copy_from_slice(&5u32.to_be_bytes()); // a tracked wire_seq
    let mut c = core();
    c.handle_datagram(&source(60, 1, 0, true, &records(&[&dg])).encode(), 1_000);
    let types = ack_types(&mut c, 1_000);
    assert!(!types.contains(&0x06), "{types:?}");
    assert_eq!(types.iter().filter(|&&t| t == 0x08).count(), 1, "{types:?}");
}

#[test]
fn h264_fragment_with_bad_index_in_a_source_is_content_rejected() {
    let mut dg = fragment_frame(4, 0, true, &[7u8; 100], 1200).remove(0);
    dg[4..6].copy_from_slice(&1u16.to_be_bytes()); // frag_idx 1 >= frag_total 1
    let mut c = core();
    c.handle_datagram(&source(70, 2, 0, true, &records(&[&dg])).encode(), 1_000);
    assert_eq!(
        status_records(&mut c, 1_000),
        vec![StatusRecord::ContentRejected(vec![(70, 0)])]
    );
}

#[test]
fn h264_fragment_dropped_after_the_ack_point_is_not_rejected() {
    // A probe pad is acknowledged and then ignored: not a refusal.
    let mut dg = fragment_frame(4, 0, true, &[7u8; 100], 1200).remove(0);
    dg[16] |= 0x10; // FRAME_FLAG_PAD
    let mut c = core();
    c.handle_datagram(&source(71, 2, 0, true, &records(&[&dg])).encode(), 1_000);
    assert!(status_records(&mut c, 1_000).is_empty());
}

#[test]
fn good_cdf53_pass_under_framing_is_neither_v3_acked_nor_rejected() {
    // Pass 0: present-passes bitmap with bit 0 set, then three empty planes.
    let mut p = vec![0x00, 0x41];
    for _ in 0..3 {
        p.extend_from_slice(&[0x00, 0x01, 0xFF]);
    }
    let t = tile_pass(3, 1, 1, 0, &p);
    let mut c = core();
    let evs = c.handle_datagram(&source(70, 1, 0, true, &records(&[&t])).encode(), 1_000);
    assert!(!evs.iter().any(|e| matches!(e, Event::DecodeError { .. })));
    assert!(status_records(&mut c, 1_000).is_empty());
    let types = ack_types(&mut c, 1_000);
    assert!(!types.contains(&0x06), "{types:?}");
    assert!(types.contains(&0x08), "{types:?}");
}

#[test]
fn late_original_after_a_rebuild_is_acked_but_not_delivered_twice() {
    let d: Vec<Vec<u8>> = (0..3).map(|i| solid(i, 0)).collect();
    let recs: Vec<Vec<u8>> = d.iter().map(|x| records(&[x])).collect();
    let srcs: Vec<SourceFrame> = recs
        .iter()
        .enumerate()
        .map(|(i, r)| source(20 + i as u32, 7, i as u8, i == 2, r))
        .collect();
    let syms: Vec<Vec<u8>> = srcs.iter().map(|s| s.coded_symbol()).collect();
    let refs: Vec<&[u8]> = syms.iter().map(|s| s.as_slice()).collect();
    let rep = encode_repairs(&refs, &[0]).unwrap();
    let repair = RepairFrame {
        bundle_seq: 99,
        block_id: 7,
        k: 3,
        r: 1,
        repair_idx: 0,
        payload: &rep[0],
    }
    .encode();
    let mut c = core();
    c.handle_datagram(&srcs[0].encode(), 1_000);
    c.handle_datagram(&srcs[2].encode(), 1_001);
    let evs = c.handle_datagram(&repair, 1_002);
    assert_eq!(only_tiles(&evs).len(), 1, "the rebuilt tile");
    // The original shows up after all.
    let late = c.handle_datagram(&srcs[1].encode(), 1_003);
    assert!(only_tiles(&late).is_empty(), "delivered twice: {late:?}");

    let mut seen: Vec<u32> = Vec::new();
    let mut outs = drain(&mut c, 1_003);
    let mut now = 1_003;
    for _ in 0..5 {
        let Some(t) = c.poll_timeout() else { break };
        now = t.max(now);
        c.on_timeout(now);
        outs.extend(drain(&mut c, now));
    }
    for o in outs {
        if let PollOutput::Datagram(dg) = o {
            if let Ok(b) = ghostframe_protocol::fec::feedback::BundleAckBatch::decode(&dg) {
                seen.extend(b.entries.iter().map(|e| e.bundle_seq));
            }
        }
    }
    assert!(seen.contains(&21), "late original is an arrival: {seen:?}");
}
