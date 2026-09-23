use ghostframe_client_gpu::dirty::{DirtyGrid, DirtyHistory};

#[test]
fn grid_records_and_reports_set_tiles() {
    let mut g = DirtyGrid::new(60, 34);
    assert!(g.is_empty());
    g.set(5, 7);
    g.set(59, 33);
    assert!(!g.is_empty());
    assert!(g.get(5, 7));
    assert!(g.get(59, 33));
    assert!(!g.get(5, 8));
    let mut set: Vec<_> = g.iter_set().collect();
    set.sort_unstable();
    assert_eq!(set, vec![(5, 7), (59, 33)]);
}

#[test]
fn history_unions_every_generation_after_the_buffer_was_filled() {
    let mut h = DirtyHistory::new(4, 4, 8);

    h.current_mut().set(0, 0);
    let g0 = h.advance();
    h.current_mut().set(1, 1);
    let g1 = h.advance();
    h.current_mut().set(2, 2);
    let _g2 = h.advance();

    // A buffer last filled at g0 must receive everything after g0.
    let u = h.union_since(Some(g0)).expect("union");
    assert!(
        !u.get(0, 0),
        "generation at or before the fill must be excluded"
    );
    assert!(u.get(1, 1));
    assert!(u.get(2, 2));

    // A buffer filled at g1 sees only what came after.
    let u = h.union_since(Some(g1)).expect("union");
    assert!(!u.get(1, 1));
    assert!(u.get(2, 2));
}

#[test]
fn never_filled_buffer_requests_a_full_blit() {
    let mut h = DirtyHistory::new(4, 4, 8);
    h.current_mut().set(0, 0);
    h.advance();
    // None means "never filled" -> caller must do a full blit.
    assert!(h.union_since(None).is_none());
}

#[test]
fn buffer_older_than_the_ring_requests_a_full_blit() {
    let mut h = DirtyHistory::new(4, 4, 4); // capacity 4
    let old = h.advance();
    for _ in 0..8 {
        h.current_mut().set(1, 1);
        h.advance();
    }
    // `old` has been evicted; we cannot reconstruct the union, so the
    // caller must blit everything rather than silently under-copying.
    assert!(
        h.union_since(Some(old)).is_none(),
        "an evicted generation must force a full blit, not a partial one"
    );
}
