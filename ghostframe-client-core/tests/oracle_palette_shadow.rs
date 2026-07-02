//! Port of `ghostframe-web-client/tests/palette_shadow.test.ts` (4 cases).

use ghostframe_client_core::palette_shadow::PaletteShadow;

#[test]
fn starts_empty() {
    let s = PaletteShadow::new();
    for i in 0..=255u8 {
        assert!(!s.has(i));
    }
}

#[test]
fn records_and_reports_presence() {
    let mut s = PaletteShadow::new();
    s.put(7, 16);
    assert!(s.has(7));
    assert_eq!(s.count(7), 16);
}

#[test]
fn overwrites_existing_entries() {
    let mut s = PaletteShadow::new();
    s.put(7, 16);
    s.put(7, 4);
    assert_eq!(s.count(7), 4);
}

#[test]
fn clear_removes_everything() {
    let mut s = PaletteShadow::new();
    s.put(7, 8);
    s.put(13, 16);
    s.clear();
    assert!(!s.has(7));
    assert!(!s.has(13));
}

#[test]
fn ignores_out_of_range_count_zero() {
    let mut s = PaletteShadow::new();
    s.put(5, 0);
    assert!(!s.has(5));
}

#[test]
fn ignores_out_of_range_count_too_large() {
    let mut s = PaletteShadow::new();
    s.put(5, 17);
    assert!(!s.has(5));
}

#[test]
fn preserves_existing_value_when_ignoring_invalid_count() {
    let mut s = PaletteShadow::new();
    s.put(5, 8);
    assert_eq!(s.count(5), 8);
    s.put(5, 0);
    assert_eq!(s.count(5), 8);
    s.put(5, 17);
    assert_eq!(s.count(5), 8);
}
