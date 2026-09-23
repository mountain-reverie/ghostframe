use ghostframe_cli::geometry::{map_pointer, Placement};

#[test]
fn a_smaller_image_is_centred() {
    let p = Placement::centre(1920, 1080, 2560, 1440);
    assert_eq!((p.origin_x, p.origin_y), (320, 180));
}

#[test]
fn an_exact_match_has_no_offset() {
    let p = Placement::centre(1920, 1080, 1920, 1080);
    assert_eq!((p.origin_x, p.origin_y), (0, 0));
}

#[test]
fn an_odd_surplus_does_not_overflow_the_output() {
    let p = Placement::centre(100, 100, 101, 101);
    assert!(p.origin_x + 100 <= 101);
    assert!(p.origin_y + 100 <= 101);
}

#[test]
fn pointer_inside_the_image_maps_by_offset() {
    let p = Placement::centre(1920, 1080, 2560, 1440);
    assert_eq!(map_pointer(&p, 320, 180), (0, 0));
    assert_eq!(map_pointer(&p, 1000, 700), (680, 520));
}

#[test]
fn pointer_in_the_black_surround_clamps_to_the_edge() {
    // The wire carries i16, so a negative coordinate is a REAL value the
    // server would act on. Clamping is not cosmetic.
    let p = Placement::centre(1920, 1080, 2560, 1440);
    assert_eq!(map_pointer(&p, 0, 0), (0, 0));
    assert_eq!(map_pointer(&p, 2559, 1439), (1919, 1079));
}

#[test]
fn an_image_larger_than_the_output_is_not_given_a_negative_origin() {
    // A remote bigger than the display: pin to 0 and crop, rather than an
    // origin the mapping would subtract into nonsense.
    let p = Placement::centre(2560, 1440, 1920, 1080);
    assert_eq!((p.origin_x, p.origin_y), (0, 0));
}

#[test]
fn a_remote_wider_than_i16_still_clamps_into_range() {
    // i16::MAX is 32767. A pathological remote width must not wrap when
    // clamped -- it should saturate.
    let p = Placement::centre(40000, 1080, 1920, 1080);
    let (x, _) = map_pointer(&p, 1919, 0);
    assert!(x >= 0, "clamped x wrapped negative: {x}");
}
