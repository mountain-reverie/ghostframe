use ghostframe_client::*; // crate lib name is ghostframe_client

#[test]
fn null_arguments_are_rejected_not_dereferenced() {
    unsafe {
        assert_eq!(
            gf_client_create(std::ptr::null(), std::ptr::null_mut()),
            gf_result::GF_ERR_INVALID
        );
        let mut out: *mut gf_client = std::ptr::null_mut();
        assert_eq!(
            gf_client_create(std::ptr::null(), &mut out),
            gf_result::GF_ERR_INVALID
        );
        assert_eq!(gf_client_event_fd(std::ptr::null()), -1);
    }
}

#[test]
fn a_wrong_struct_size_is_rejected() {
    // The forward-compatibility mechanism must actually work, or a future
    // ABI change silently reads garbage out of a shorter struct.
    let mut cfg: gf_client_config = unsafe { std::mem::zeroed() };
    cfg.struct_size = 4; // deliberately wrong
    let mut out: *mut gf_client = std::ptr::null_mut();
    unsafe {
        assert_eq!(gf_client_create(&cfg, &mut out), gf_result::GF_ERR_INVALID);
    }
}

/// A `struct_size` reporting the struct as it existed before
/// `max_decode_width`/`max_decode_height` were appended -- standing in for
/// a caller built against an older header -- must be accepted, not
/// rejected: that is the whole point of appending fields rather than
/// breaking `struct_size`'s exact-match check. Companion to
/// `a_wrong_struct_size_is_rejected`: that test proves too-small is still
/// rejected below the original struct's size; this one proves "smaller
/// than today, but at least the original size" is NOT lumped in with
/// "wrong".
#[test]
fn an_older_smaller_struct_size_is_accepted_with_prewarm_defaulted() {
    let host = std::ffi::CString::new("test").unwrap();
    let dir = std::ffi::CString::new("/tmp/gf-capi-test-oldcfg").unwrap();
    let key = std::ffi::CString::new("").unwrap();
    let mut cfg: gf_client_config = unsafe { std::mem::zeroed() };
    cfg.struct_size = std::mem::offset_of!(gf_client_config, max_decode_width) as u32;
    cfg.hostname = host.as_ptr();
    cfg.state_dir = dir.as_ptr();
    cfg.authkey = key.as_ptr();
    cfg.n_export_buffers = 3;
    // An old caller's real allocation has no bytes here at all; a non-zero
    // sentinel confirms the library actually gates on `struct_size` rather
    // than merely defaulting because these happened to be zeroed.
    cfg.max_decode_width = 0xdead_beef;
    cfg.max_decode_height = 0xdead_beef;
    let mut out: *mut gf_client = std::ptr::null_mut();
    unsafe {
        assert_eq!(
            gf_client_create(&cfg, &mut out),
            gf_result::GF_OK,
            "a struct_size matching an older header must be accepted, not rejected"
        );
        assert!(!out.is_null());
        gf_client_destroy(out);
    }
}

#[test]
fn version_is_reported() {
    let (mut a, mut b, mut c) = (0u32, 0u32, 0u32);
    unsafe { gf_version(&mut a, &mut b, &mut c) };
    assert!(a + b + c > 0, "version is all zeroes");
}

#[test]
fn result_strings_are_non_null_and_distinct() {
    let codes = [
        gf_result::GF_OK,
        gf_result::GF_AGAIN,
        gf_result::GF_ERR_INVALID,
        gf_result::GF_ERR_STATE,
        gf_result::GF_ERR_IO,
        gf_result::GF_ERR_GPU,
    ];
    let mut seen = std::collections::HashSet::new();
    for r in codes {
        let p = unsafe { gf_result_str(r) };
        assert!(!p.is_null());
        let s = unsafe { std::ffi::CStr::from_ptr(p) }
            .to_str()
            .expect("utf8");
        assert!(seen.insert(s.to_string()), "duplicate string for {s}");
    }
}

#[test]
fn create_and_destroy_without_connecting_is_clean() {
    let host = std::ffi::CString::new("test").unwrap();
    let dir = std::ffi::CString::new("/tmp/gf-capi-test").unwrap();
    let key = std::ffi::CString::new("").unwrap();
    let mut cfg: gf_client_config = unsafe { std::mem::zeroed() };
    cfg.struct_size = std::mem::size_of::<gf_client_config>() as u32;
    cfg.hostname = host.as_ptr();
    cfg.state_dir = dir.as_ptr();
    cfg.authkey = key.as_ptr();
    cfg.n_export_buffers = 3;
    let mut out: *mut gf_client = std::ptr::null_mut();
    unsafe {
        assert_eq!(gf_client_create(&cfg, &mut out), gf_result::GF_OK);
        assert!(!out.is_null());
        assert!(gf_client_event_fd(out) >= 0);
        // Draining an idle client must report GF_AGAIN, not block or error.
        let mut ev: gf_event = std::mem::zeroed();
        ev.struct_size = std::mem::size_of::<gf_event>() as u32;
        assert_eq!(gf_client_next_event(out, &mut ev), gf_result::GF_AGAIN);
        gf_client_destroy(out);
    }
}
