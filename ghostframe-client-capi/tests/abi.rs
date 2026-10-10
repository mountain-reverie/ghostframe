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
        let p = unsafe { gf_result_str(r as u32) };
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

#[test]
fn result_str_survives_a_value_that_is_not_a_gf_result() {
    // A C caller can hand over any integer; a Rust enum by value would make
    // that undefined behaviour.
    for bad in [6u32, 1000, u32::MAX] {
        let p = unsafe { gf_result_str(bad) };
        let s = unsafe { std::ffi::CStr::from_ptr(p) }.to_str().unwrap();
        assert_eq!(s, "GF_ERR_UNKNOWN");
    }
}

fn make_client() -> *mut gf_client {
    let host = std::ffi::CString::new("test").unwrap();
    let dir = std::ffi::CString::new("/tmp/gf-capi-test").unwrap();
    let key = std::ffi::CString::new("").unwrap();
    let mut cfg: gf_client_config = unsafe { std::mem::zeroed() };
    cfg.struct_size = std::mem::size_of::<gf_client_config>() as u32;
    cfg.hostname = host.as_ptr();
    cfg.state_dir = dir.as_ptr();
    cfg.authkey = key.as_ptr();
    let mut out: *mut gf_client = std::ptr::null_mut();
    assert_eq!(
        unsafe { gf_client_create(&cfg, &mut out) },
        gf_result::GF_OK
    );
    out
}

#[test]
fn a_wrong_event_or_frame_struct_size_is_rejected_without_writing() {
    let c = make_client();
    unsafe {
        let mut ev: gf_event = std::mem::zeroed();
        ev.struct_size = 8;
        ev.width = 0xdead;
        assert_eq!(gf_client_next_event(c, &mut ev), gf_result::GF_ERR_INVALID);
        assert_eq!(ev.width, 0xdead, "rejected call wrote to the struct");
        let mut fr: gf_frame = std::mem::zeroed();
        fr.struct_size = 8;
        assert_eq!(
            gf_client_acquire_frame(c, &mut fr),
            gf_result::GF_ERR_INVALID
        );
        gf_client_destroy(c);
    }
}

#[test]
fn oversized_and_misaligned_config_arrays_are_rejected() {
    let host = std::ffi::CString::new("test").unwrap();
    let dir = std::ffi::CString::new("/tmp/gf-capi-test").unwrap();
    let key = std::ffi::CString::new("").unwrap();
    let mods = [0u64; 4];
    let mut cfg: gf_client_config = unsafe { std::mem::zeroed() };
    cfg.struct_size = std::mem::size_of::<gf_client_config>() as u32;
    cfg.hostname = host.as_ptr();
    cfg.state_dir = dir.as_ptr();
    cfg.authkey = key.as_ptr();
    let mut out: *mut gf_client = std::ptr::null_mut();
    unsafe {
        cfg.n_export_buffers = 1_000_000;
        assert_eq!(gf_client_create(&cfg, &mut out), gf_result::GF_ERR_INVALID);
        cfg.n_export_buffers = 3;
        cfg.n_preferred_modifiers = u32::MAX;
        cfg.preferred_modifiers = mods.as_ptr();
        assert_eq!(gf_client_create(&cfg, &mut out), gf_result::GF_ERR_INVALID);
        cfg.n_preferred_modifiers = 1;
        cfg.preferred_modifiers = (mods.as_ptr() as *const u8).add(1).cast();
        assert_eq!(gf_client_create(&cfg, &mut out), gf_result::GF_ERR_INVALID);
        cfg.preferred_modifiers = mods.as_ptr();
        assert_eq!(gf_client_create(&cfg, &mut out), gf_result::GF_OK);
        gf_client_destroy(out);
    }
}

#[test]
fn input_and_display_mode_before_connect_report_state_error() {
    let c = make_client();
    unsafe {
        assert_eq!(
            gf_client_request_display_mode(c, 1280, 720),
            gf_result::GF_ERR_STATE
        );
        assert_eq!(gf_client_push_key(c, 65, true), gf_result::GF_ERR_STATE);
        assert_eq!(gf_client_push_wheel(c, 0, 1), gf_result::GF_ERR_STATE);
        assert_eq!(
            gf_client_request_display_mode(std::ptr::null(), 1, 1),
            gf_result::GF_ERR_INVALID
        );
        gf_client_destroy(c);
    }
}

#[test]
fn release_of_a_frame_never_acquired_is_inert() {
    let c = make_client();
    unsafe {
        gf_client_release_frame(c, 0);
        gf_client_release_frame(c, 12345);
        gf_client_release_frame(std::ptr::null(), 0);
        gf_client_destroy(c);
    }
}

#[test]
fn calls_from_many_threads_on_one_handle_are_safe() {
    // The threading contract: everything but destroy is callable
    // concurrently. Under the old `&mut` shim this was aliasing UB.
    struct P(*mut gf_client);
    unsafe impl Send for P {}
    unsafe impl Sync for P {}
    let c = make_client();
    let p = std::sync::Arc::new(P(c));
    let fd = unsafe { gf_client_event_fd(c) };
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let p = p.clone();
            std::thread::spawn(move || {
                for i in 0..500u32 {
                    unsafe {
                        let mut ev: gf_event = std::mem::zeroed();
                        ev.struct_size = std::mem::size_of::<gf_event>() as u32;
                        assert_eq!(gf_client_next_event(p.0, &mut ev), gf_result::GF_AGAIN);
                        let mut fr: gf_frame = std::mem::zeroed();
                        fr.struct_size = std::mem::size_of::<gf_frame>() as u32;
                        assert_eq!(gf_client_acquire_frame(p.0, &mut fr), gf_result::GF_AGAIN);
                        gf_client_release_frame(p.0, i);
                        assert_eq!(gf_client_event_fd(p.0), fd);
                        assert_eq!(gf_client_push_key(p.0, i, true), gf_result::GF_ERR_STATE);
                    }
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    unsafe { gf_client_destroy(c) };
}
