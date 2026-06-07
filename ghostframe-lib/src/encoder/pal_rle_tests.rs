use super::*;

#[test]
fn palette_entry_default_is_empty() {
    let p = PaletteEntry::default();
    assert_eq!(p.count, 0);
    assert_eq!(p.colors[0], [0, 0, 0, 0]);
}

#[test]
fn slot_state_variants_distinct() {
    assert_ne!(SlotState::Empty, SlotState::Held);
    assert_ne!(SlotState::Held, SlotState::FreeButCached);
}

fn make_palette(colors: &[[u8; 4]]) -> PaletteEntry {
    let mut e = PaletteEntry::default();
    for (i, c) in colors.iter().enumerate() {
        e.colors[i] = *c;
    }
    e.count = colors.len() as u8;
    e
}

#[test]
fn empty_table_has_no_matches() {
    let t = PaletteTable::new();
    let p = make_palette(&[[10, 20, 30, 255]]);
    assert_eq!(t.find_matching(&p), None);
}

#[test]
fn acquire_promotes_free_but_cached_to_held() {
    let mut t = PaletteTable::new();
    let p = make_palette(&[[10, 20, 30, 255], [40, 50, 60, 255]]);
    // Manually inject for this test — full allocate ladder comes in Task 4.
    t.entries[7] = Some(p);
    t.slot_state[7] = SlotState::FreeButCached;
    t.acquire(7);
    assert_eq!(t.slot_state[7], SlotState::Held);
    assert_eq!(t.ref_count[7], 1);
}

#[test]
fn release_drops_to_free_but_cached_at_zero() {
    let mut t = PaletteTable::new();
    let p = make_palette(&[[1, 2, 3, 255]]);
    t.entries[3] = Some(p);
    t.slot_state[3] = SlotState::FreeButCached;
    t.acquire(3);
    t.acquire(3);
    t.release(3);
    assert_eq!(t.slot_state[3], SlotState::Held);
    assert_eq!(t.ref_count[3], 1);
    t.release(3);
    assert_eq!(t.slot_state[3], SlotState::FreeButCached);
    assert_eq!(t.ref_count[3], 0);
    assert!(
        t.free_lru.contains(&3),
        "release at ref_count=0 must push id to free_lru"
    );
}

#[test]
fn acquire_removes_id_from_free_lru() {
    let mut t = PaletteTable::new();
    let p1 = make_palette(&[[1, 1, 1, 255]]);
    let p2 = make_palette(&[[2, 2, 2, 255]]);
    // Two FreeButCached slots in free_lru.
    t.entries[10] = Some(p1);
    t.slot_state[10] = SlotState::FreeButCached;
    t.free_lru.push_back(10);
    t.entries[20] = Some(p2);
    t.slot_state[20] = SlotState::FreeButCached;
    t.free_lru.push_back(20);
    // Re-acquire slot 10; it must leave free_lru while 20 stays.
    t.acquire(10);
    assert!(
        !t.free_lru.contains(&10),
        "acquired slot must be removed from free_lru"
    );
    assert!(t.free_lru.contains(&20), "other free slot must remain");
    assert_eq!(t.slot_state[10], SlotState::Held);
    assert_eq!(t.ref_count[10], 1);
}

#[test]
fn find_matching_hits_held_slots() {
    let mut t = PaletteTable::new();
    let p = make_palette(&[[10, 20, 30, 255], [40, 50, 60, 255]]);
    t.entries[5] = Some(p);
    t.slot_state[5] = SlotState::Held;
    assert_eq!(t.find_matching(&p), Some(5));
}

#[test]
fn find_matching_hits_free_but_cached_slots() {
    let mut t = PaletteTable::new();
    let p = make_palette(&[[10, 20, 30, 255]]);
    t.entries[9] = Some(p);
    t.slot_state[9] = SlotState::FreeButCached;
    assert_eq!(t.find_matching(&p), Some(9));
}

#[test]
fn find_matching_ignores_empty_slots() {
    let mut t = PaletteTable::new();
    let p = make_palette(&[[10, 20, 30, 255]]);
    // entries[4] left None; slot_state[4] is Empty.
    t.entries[4] = None;
    t.slot_state[4] = SlotState::Empty;
    assert_eq!(t.find_matching(&p), None);
}

#[test]
fn overwrite_eligible_requires_zero_ref_count() {
    let mut t = PaletteTable::new();
    let p = make_palette(&[[1, 2, 3, 255]]);
    t.entries[2] = Some(p);
    t.slot_state[2] = SlotState::Held;
    t.ref_count[2] = 1;
    assert!(
        !t.overwrite_eligible(2),
        "ref_count > 0 must block overwrite"
    );
}

#[test]
fn overwrite_eligible_passes_when_delivered() {
    let mut t = PaletteTable::new();
    t.entries[3] = Some(make_palette(&[[1, 2, 3, 255]]));
    t.slot_state[3] = SlotState::FreeButCached;
    t.delivered.insert(3);
    assert!(t.overwrite_eligible(3));
}

#[test]
fn overwrite_eligible_passes_when_never_sent() {
    let mut t = PaletteTable::new();
    t.entries[4] = Some(make_palette(&[[1, 2, 3, 255]]));
    t.slot_state[4] = SlotState::FreeButCached;
    // delivered = false, in_flight_carrying = 0  → never-sent case.
    assert!(t.overwrite_eligible(4));
}

#[test]
fn overwrite_eligible_blocked_when_in_flight_not_delivered() {
    let mut t = PaletteTable::new();
    t.entries[5] = Some(make_palette(&[[1, 2, 3, 255]]));
    t.slot_state[5] = SlotState::FreeButCached;
    t.in_flight_carrying[5] = 1;
    assert!(!t.overwrite_eligible(5));
}

#[test]
fn write_bytes_replaces_entry_and_resets_to_held() {
    let mut t = PaletteTable::new();
    let new_pal = make_palette(&[[9, 9, 9, 255], [10, 10, 10, 255]]);
    t.write_bytes(11, &new_pal);
    assert_eq!(t.entries[11], Some(new_pal));
    assert_eq!(t.slot_state[11], SlotState::Held);
    assert_eq!(t.ref_count[11], 0);
    assert!(!t.delivered.contains(11));
    assert_eq!(t.in_flight_carrying[11], 0);
}

#[test]
fn find_empty_slot_returns_lowest_empty() {
    let mut t = PaletteTable::new();
    // Mark slot 0 as Held to force scan past it.
    t.entries[0] = Some(make_palette(&[[1, 1, 1, 255]]));
    t.slot_state[0] = SlotState::Held;
    assert_eq!(t.find_empty_slot(), Some(1));
}

#[test]
fn find_empty_slot_returns_none_when_full() {
    let mut t = PaletteTable::new();
    for id in 0..PALETTE_TABLE_SLOTS {
        t.slot_state[id] = SlotState::Held;
        t.entries[id] = Some(make_palette(&[[id as u8, 0, 0, 255]]));
    }
    assert_eq!(t.find_empty_slot(), None);
}

#[test]
fn find_eligible_free_slot_picks_lru_head() {
    let mut t = PaletteTable::new();
    for id in [7u8, 13, 22] {
        t.entries[id as usize] = Some(make_palette(&[[id, 0, 0, 255]]));
        t.slot_state[id as usize] = SlotState::FreeButCached;
        t.delivered.insert(id); // make all overwrite-eligible
        t.free_lru.push_back(id);
    }
    assert_eq!(t.find_eligible_free_slot(), Some(7));
}

#[test]
fn find_eligible_free_slot_skips_ineligible_entries() {
    let mut t = PaletteTable::new();
    // 7: in_flight, not delivered → ineligible
    t.entries[7] = Some(make_palette(&[[7, 0, 0, 255]]));
    t.slot_state[7] = SlotState::FreeButCached;
    t.in_flight_carrying[7] = 1;
    t.free_lru.push_back(7);
    // 13: delivered → eligible
    t.entries[13] = Some(make_palette(&[[13, 0, 0, 255]]));
    t.slot_state[13] = SlotState::FreeButCached;
    t.delivered.insert(13);
    t.free_lru.push_back(13);
    assert_eq!(t.find_eligible_free_slot(), Some(13));
}

#[test]
fn acquire_or_allocate_returns_match_when_present() {
    let mut t = PaletteTable::new();
    let p = make_palette(&[[1, 2, 3, 255]]);
    t.entries[42] = Some(p);
    t.slot_state[42] = SlotState::FreeButCached;
    let id = t.acquire_or_allocate(&p).unwrap();
    assert_eq!(id, 42);
    assert_eq!(t.slot_state[42], SlotState::Held);
    assert_eq!(t.ref_count[42], 1);
}

#[test]
fn acquire_or_allocate_uses_eligible_free_slot_when_no_match() {
    let mut t = PaletteTable::new();
    // Pre-populate slot 9 with a different palette, FreeButCached + delivered.
    let old = make_palette(&[[1, 2, 3, 255]]);
    t.entries[9] = Some(old);
    t.slot_state[9] = SlotState::FreeButCached;
    t.delivered.insert(9);
    t.free_lru.push_back(9);

    // Mark ALL other slots as Held so find_empty_slot returns None
    // and the 4-way ladder is forced to step 3 (find_eligible_free_slot).
    // (Post-fix ladder: find_matching → find_empty_slot → find_eligible_free_slot.
    //  Step 2 must fail before step 3 fires.)
    for id in 0..PALETTE_TABLE_SLOTS {
        if id == 9 {
            continue;
        }
        t.slot_state[id] = SlotState::Held;
        t.entries[id] = Some(make_palette(&[[(id as u8).wrapping_mul(3), 7, 7, 255]]));
    }
    let new_pal = make_palette(&[[99, 99, 99, 255]]);
    let id = t.acquire_or_allocate(&new_pal).unwrap();
    assert_eq!(id, 9);
    assert_eq!(t.entries[9], Some(new_pal));
    assert!(!t.delivered.contains(9));
    assert_eq!(t.slot_state[9], SlotState::Held);
    assert_eq!(t.ref_count[9], 1);
    assert!(!t.free_lru.contains(&9));
}

#[test]
fn acquire_or_allocate_uses_empty_slot_when_no_match_no_eligible() {
    let mut t = PaletteTable::new();
    let p = make_palette(&[[5, 5, 5, 255]]);
    let id = t.acquire_or_allocate(&p).unwrap();
    assert_eq!(id, 0); // first empty
    assert_eq!(t.entries[0], Some(p));
    assert_eq!(t.slot_state[0], SlotState::Held);
    assert_eq!(t.ref_count[0], 1);
}

#[test]
fn acquire_or_allocate_uses_empty_slots_before_evicting_cached() {
    let mut t = PaletteTable::new();
    let p_red = {
        let mut p = PaletteEntry::default();
        p.count = 1;
        p.colors[0] = [0, 0, 0xFF, 0xFF];
        p
    };
    let p_blue = {
        let mut p = PaletteEntry::default();
        p.count = 1;
        p.colors[0] = [0xFF, 0, 0, 0xFF];
        p
    };

    // Frame 1: red. Lands in empty slot 0.
    let id_red = t.acquire_or_allocate(&p_red).expect("alloc red");
    assert_eq!(id_red, 0);
    t.release(id_red); // end-of-frame release
    t.delivered.insert(id_red); // simulate ACK arrival

    // Frame 2: blue. MUST land in empty slot 1, NOT overwrite slot 0.
    // (This is the regression we're testing — pre-fix, find_eligible_free_slot
    // would return slot 0 as oldest LRU and write_bytes would clear
    // delivered[0]. Post-fix, find_empty_slot returns slot 1 first.)
    let id_blue = t.acquire_or_allocate(&p_blue).expect("alloc blue");
    assert_eq!(
        id_blue, 1,
        "blue should land in empty slot 1, not overwrite slot 0"
    );
    assert!(
        t.delivered.contains(id_red),
        "delivered on slot 0 must survive the slot 1 allocation"
    );
    t.release(id_blue);
    t.delivered.insert(id_blue);

    // Frame 3: red again. find_matching hits slot 0; no write_bytes call.
    let id_red_2 = t.acquire_or_allocate(&p_red).expect("re-alloc red");
    assert_eq!(id_red_2, 0);
    assert!(
        t.delivered.contains(0),
        "find_matching path must preserve delivered"
    );
    t.release(id_red_2);

    // Frame 4: blue again. find_matching hits slot 1.
    let id_blue_2 = t.acquire_or_allocate(&p_blue).expect("re-alloc blue");
    assert_eq!(id_blue_2, 1);
    assert!(t.delivered.contains(1));
}

#[test]
fn acquire_or_allocate_returns_none_when_full_and_all_ineligible() {
    let mut t = PaletteTable::new();
    for id in 0..PALETTE_TABLE_SLOTS {
        let p = make_palette(&[[id as u8, 0, 0, 255]]);
        t.entries[id] = Some(p);
        t.slot_state[id] = SlotState::Held;
        t.ref_count[id] = 1;
    }
    let new_pal = make_palette(&[[200, 200, 200, 255]]);
    assert_eq!(t.acquire_or_allocate(&new_pal), None);
}

#[test]
fn on_session_reset_preserves_bytes_clears_tracking() {
    let mut t = PaletteTable::new();
    let p = make_palette(&[[1, 2, 3, 255]]);
    t.entries[7] = Some(p);
    t.slot_state[7] = SlotState::Held;
    t.ref_count[7] = 3;
    t.delivered.insert(7);
    t.in_flight_carrying[7] = 2;

    t.on_session_reset(false);

    assert_eq!(t.entries[7], Some(p), "bytes preserved (warm cache)");
    assert_eq!(t.slot_state[7], SlotState::FreeButCached);
    assert_eq!(t.ref_count[7], 0);
    assert!(!t.delivered.contains(7));
    assert_eq!(t.in_flight_carrying[7], 0);
}

#[test]
fn on_session_reset_preserve_delivered_keeps_bit() {
    let mut t = PaletteTable::new();
    // Allocate and "deliver" a palette.
    let p = PaletteEntry {
        count: 2,
        colors: {
            let mut c = [[0u8; 4]; 16];
            c[0] = [0xFF, 0, 0, 0xFF];
            c[1] = [0, 0, 0xFF, 0xFF];
            c
        },
    };
    let id = t.acquire_or_allocate(&p).unwrap();
    t.delivered.insert(id);
    t.in_flight_carrying[id as usize] = 3;
    t.ref_count[id as usize] = 1;
    assert!(t.delivered.contains(id), "precondition: delivered set");

    // preserve_delivered=true: delivered bit stays, other per-session state still resets.
    t.on_session_reset(true);
    assert!(
        t.delivered.contains(id),
        "preserve_delivered=true must keep the delivered bit set"
    );
    assert_eq!(
        t.in_flight_carrying[id as usize], 0,
        "in_flight_carrying still resets"
    );
    assert_eq!(t.ref_count[id as usize], 0, "ref_count still resets");
    assert!(
        t.entries[id as usize].is_some(),
        "palette bytes preserved (warm cache)"
    );
}

#[test]
fn on_session_reset_keeps_empty_slots_empty() {
    let mut t = PaletteTable::new();
    // slot 99 untouched
    t.on_session_reset(false);
    assert_eq!(t.slot_state[99], SlotState::Empty);
    assert!(t.entries[99].is_none());
}

#[test]
fn on_session_reset_rebuilds_free_lru_with_cached_slots() {
    let mut t = PaletteTable::new();
    let p1 = make_palette(&[[1, 1, 1, 255]]);
    let p2 = make_palette(&[[2, 2, 2, 255]]);
    t.entries[5] = Some(p1);
    t.slot_state[5] = SlotState::Held;
    t.entries[8] = Some(p2);
    t.slot_state[8] = SlotState::FreeButCached;

    t.on_session_reset(false);

    // Both 5 and 8 become FreeButCached; free_lru contains both.
    assert!(t.free_lru.contains(&5));
    assert!(t.free_lru.contains(&8));
}

#[test]
fn find_matching_still_hits_after_reset() {
    let mut t = PaletteTable::new();
    let p = make_palette(&[[10, 20, 30, 255]]);
    t.entries[42] = Some(p);
    t.slot_state[42] = SlotState::Held;
    t.delivered.insert(42);
    t.in_flight_carrying[42] = 1;

    t.on_session_reset(false);
    // Warm cache still finds the palette by bytes.
    assert_eq!(t.find_matching(&p), Some(42));
}

fn make_indices_4bit(indices: &[u8]) -> [u8; 512] {
    // Packs `indices` (one nibble each, low nibble of byte 0 first) into 512 bytes.
    assert!(indices.len() <= 1024);
    let mut out = [0u8; 512];
    for (i, &idx) in indices.iter().enumerate() {
        let byte = i / 2;
        let shift = (i % 2) * 4;
        out[byte] |= (idx & 0x0F) << shift;
    }
    out
}

#[test]
fn rle_all_same_color_compresses_to_64_bytes() {
    // 1024 pixels all index 5 → 64 runs of length 16 → 64 RLE bytes.
    let indices = vec![5u8; 1024];
    let packed = make_indices_4bit(&indices);
    let rle = encode_pal_rle_indices(&packed);
    assert_eq!(rle.len(), 64);
    for &b in &rle {
        assert_eq!(b, (5 << 4) | 15, "each byte should be (idx 5, run 16)");
    }
}

#[test]
fn rle_alternating_colors_emits_one_byte_per_pixel() {
    // Alternating 0, 1, 0, 1, ... — every pixel is a run-of-1.
    let indices: Vec<u8> = (0..1024).map(|i| (i % 2) as u8).collect();
    let packed = make_indices_4bit(&indices);
    let rle = encode_pal_rle_indices(&packed);
    assert_eq!(rle.len(), 1024);
    assert_eq!(rle[0], 0x00);
    assert_eq!(rle[1], 0x10);
    assert_eq!(rle[2], 0x00);
}

#[test]
fn rle_run_caps_at_16() {
    let mut indices = vec![3u8; 17];
    indices.extend(vec![0u8; 1024 - 17]);
    let packed = make_indices_4bit(&indices);
    let rle = encode_pal_rle_indices(&packed);
    assert_eq!(rle[0], 0x3F);
    assert_eq!(rle[1], 0x30);
}

#[test]
fn rle_pair_pattern_correct_offsets() {
    let mut indices = vec![0u8, 0, 0, 1, 1];
    indices.extend(vec![0u8; 1024 - 5]);
    let packed = make_indices_4bit(&indices);
    let rle = encode_pal_rle_indices(&packed);
    assert_eq!(rle[0], (0 << 4) | 2, "run of 3 zeros = len 3 → encoded 2");
    assert_eq!(rle[1], (1 << 4) | 1, "run of 2 ones  = len 2 → encoded 1");
}

#[test]
fn payload_bundled_layout_has_flag_id_count_palette_then_rle() {
    let palette = make_palette(&[[10, 20, 30, 255], [40, 50, 60, 255]]);
    let indices = vec![0u8; 1024];
    let packed = make_indices_4bit(&indices);
    let payload = encode_pal_rle_payload(&packed, &palette, 7, true);

    assert_eq!(payload[0] & 0x01, 0x01, "bundle flag set");
    assert_eq!(payload[1], 7, "palette id");
    assert_eq!(payload[2], 2, "count = 2");
    // Colors block: 2 entries × 4 bytes = 8 bytes starting at offset 3.
    assert_eq!(&payload[3..7], &[10, 20, 30, 255]);
    assert_eq!(&payload[7..11], &[40, 50, 60, 255]);
    // RLE bytes after offset 11.
    assert!(payload.len() > 11);
}

#[test]
fn payload_thin_layout_skips_palette_block() {
    let palette = make_palette(&[[10, 20, 30, 255]]);
    let indices = vec![0u8; 1024];
    let packed = make_indices_4bit(&indices);
    let payload = encode_pal_rle_payload(&packed, &palette, 42, false);

    assert_eq!(payload[0] & 0x01, 0x00, "bundle flag clear");
    assert_eq!(payload[1], 42, "palette id");
    // RLE starts immediately at offset 2.
    // All-same → 64 bytes RLE → total 66 bytes.
    assert_eq!(payload.len(), 66);
}

#[test]
fn decode_bundled_extracts_palette() {
    let palette = make_palette(&[[1, 2, 3, 255], [4, 5, 6, 255]]);
    let indices: Vec<u8> = (0..1024).map(|i| (i % 2) as u8).collect();
    let packed = make_indices_4bit(&indices);
    let payload = encode_pal_rle_payload(&packed, &palette, 9, true);

    let result = decode_pal_rle(&payload, None).unwrap();
    // Pixel 0 → palette[0] = [1,2,3,255]; pixel 1 → palette[1].
    assert_eq!(&result.pixels[0..4], &[1, 2, 3, 255]);
    assert_eq!(&result.pixels[4..8], &[4, 5, 6, 255]);
    assert_eq!(result.updated_palette, Some((9, palette)));
}

#[test]
fn decode_thin_requires_cached_palette() {
    let palette = make_palette(&[[10, 20, 30, 255]]);
    let indices = vec![0u8; 1024];
    let packed = make_indices_4bit(&indices);
    let payload = encode_pal_rle_payload(&packed, &palette, 5, false);

    // No cached palette → error.
    let err = decode_pal_rle(&payload, None).unwrap_err();
    assert_eq!(err, PalRleDecodeError::UncachedPalette(5));

    // With cached palette → ok.
    let result = decode_pal_rle(&payload, Some(&palette)).unwrap();
    assert_eq!(&result.pixels[0..4], &[10, 20, 30, 255]);
    assert!(result.updated_palette.is_none());
}

#[test]
fn decode_rejects_index_out_of_range() {
    // Build a payload by hand: thin flag, palette_id=0, then a byte (idx=3, run=0)
    // when the cached palette only has 2 entries.
    let palette = make_palette(&[[1, 2, 3, 255], [4, 5, 6, 255]]);
    let mut payload = vec![0x00u8, 0]; // thin, id=0
                                       // 1024 indices = 1024 pixels worth. (idx=3, run=0) covers 1 pixel. Need 1023 more.
    payload.push((3 << 4) | 0);
    payload.push((0 << 4) | 15); // run of 16
    for _ in 0..((1024 - 1 - 16) / 16) {
        payload.push((0 << 4) | 15);
    }
    // Remainder: (1024 - 1 - 16 - (62*16)) = 1024 - 1 - 16 - 992 = 15
    payload.push((0 << 4) | 14);
    let err = decode_pal_rle(&payload, Some(&palette)).unwrap_err();
    assert!(matches!(err, PalRleDecodeError::IndexOutOfRange { .. }));
}

#[test]
fn encode_decode_roundtrip_exact_pixels() {
    let palette = make_palette(&[[10, 20, 30, 255], [40, 50, 60, 255], [70, 80, 90, 255]]);
    // 1024 pixels with arbitrary palette indices.
    let indices: Vec<u8> = (0..1024).map(|i| (i % 3) as u8).collect();
    let packed = make_indices_4bit(&indices);
    let payload = encode_pal_rle_payload(&packed, &palette, 11, true);
    let result = decode_pal_rle(&payload, None).unwrap();
    for pixel in 0..1024 {
        let expected = palette.colors[indices[pixel] as usize];
        let off = pixel * 4;
        assert_eq!(&result.pixels[off..off + 4], &expected, "pixel {}", pixel);
    }
}

use proptest::prelude::*;

fn arb_palette() -> impl Strategy<Value = PaletteEntry> {
    (1u8..=16).prop_flat_map(|count| {
        prop::collection::vec(any::<[u8; 4]>(), count as usize..=count as usize).prop_map(
            move |cols| {
                let mut p = PaletteEntry::default();
                for (i, c) in cols.iter().enumerate() {
                    p.colors[i] = *c;
                }
                p.count = count;
                p
            },
        )
    })
}

fn arb_indices_against(count: u8) -> impl Strategy<Value = [u8; 512]> {
    prop::collection::vec(0u8..count, 1024..=1024).prop_map(|v| {
        let mut out = [0u8; 512];
        for (i, &idx) in v.iter().enumerate() {
            let byte = i / 2;
            let shift = (i % 2) * 4;
            out[byte] |= (idx & 0x0F) << shift;
        }
        out
    })
}

#[test]
fn force_rebundle_clears_delivered_bit() {
    let mut t = PaletteTable::new();
    let pal = PaletteEntry {
        count: 1,
        colors: [[10, 20, 30, 255]; 16],
    };
    // Force a slot to delivered state.
    t.write_bytes(5, &pal);
    t.delivered.insert(5);
    assert!(t.delivered.contains(5));

    t.force_rebundle(5);

    assert!(
        !t.delivered.contains(5),
        "force_rebundle must clear the delivered bit so the next emission re-bundles"
    );
    // The palette content stays put — we only cleared the delivered flag.
    assert_eq!(t.entries[5].as_ref().unwrap().count, 1);
    assert_eq!(t.entries[5].as_ref().unwrap().colors[0], [10, 20, 30, 255]);
}

#[test]
fn force_rebundle_no_op_on_undelivered() {
    let mut t = PaletteTable::new();
    // Slot 7 is empty / undelivered.
    assert!(!t.delivered.contains(7));
    t.force_rebundle(7);
    // Should be a no-op, no panic.
    assert!(!t.delivered.contains(7));
}

#[test]
fn indices_raw_payload_layout_thin() {
    // Thin variant: flags = 0x02 (bit 1 set, bit 0 clear), then palette_id,
    // then 512 bytes of raw 4-bit-packed indices.
    let mut packed = [0u8; 512];
    // Distinct pattern so we can verify the bytes round-trip verbatim.
    for (i, b) in packed.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(3);
    }
    let payload = encode_pal_rle_payload_indices_raw(&packed, 42);
    assert_eq!(
        payload.len(),
        514,
        "[flags=1][palette_id=1][indices=512] = 514 bytes"
    );
    assert_eq!(payload[0], 0x02, "flags bit 1 set, bit 0 clear");
    assert_eq!(payload[1], 42, "palette_id");
    assert_eq!(
        &payload[2..],
        &packed[..],
        "indices block must be verbatim copy of packed_indices"
    );
}

proptest! {
    #[test]
    fn proptest_encode_decode_bundled_roundtrip(
        (palette, packed) in arb_palette().prop_flat_map(|p| {
            let count = p.count;
            arb_indices_against(count).prop_map(move |packed| (p, packed))
        })
    ) {
        let payload = encode_pal_rle_payload(&packed, &palette, 0, true);
        let dec = decode_pal_rle(&payload, None).unwrap();

        // Reconstruct expected pixels via direct index lookup.
        let mut expected = vec![0u8; 1024 * 4];
        for pixel in 0..1024usize {
            let byte = packed[pixel / 2];
            let idx = if pixel % 2 == 0 { byte & 0x0F } else { (byte >> 4) & 0x0F };
            let mut color = palette.colors[idx as usize];
            if color[3] == 0 { color[3] = 255; }
            expected[pixel * 4..pixel * 4 + 4].copy_from_slice(&color);
        }
        prop_assert_eq!(dec.pixels, expected);
        prop_assert_eq!(dec.updated_palette, Some((0u8, palette)));
    }

    #[test]
    fn proptest_palette_table_acquire_release_balanced(
        ops in prop::collection::vec(0u8..32u8, 1..50)
    ) {
        let mut t = PaletteTable::new();
        // Pre-allocate slots 0..32 with synthetic palettes.
        for id in 0..32u8 {
            let mut p = PaletteEntry::default();
            p.colors[0] = [id, 0, 0, 255];
            p.count = 1;
            t.entries[id as usize] = Some(p);
            t.slot_state[id as usize] = SlotState::FreeButCached;
            t.free_lru.push_back(id);
            t.delivered.insert(id);
        }
        let mut acquired: Vec<u8> = Vec::new();
        for op in ops {
            t.acquire(op);
            acquired.push(op);
        }
        // Release in reverse order; ref_counts should reach zero everywhere.
        for &id in acquired.iter().rev() {
            t.release(id);
        }
        for id in 0..32 {
            prop_assert_eq!(t.ref_count[id], 0);
            prop_assert_eq!(t.slot_state[id], SlotState::FreeButCached);
        }
    }
}
