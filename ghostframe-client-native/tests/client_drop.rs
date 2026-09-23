use ghostframe_client_native::{Client, Config};

#[test]
fn client_drop_without_connect_is_clean() {
    // Creating a client starts no threads until connect(); dropping it must
    // not hang or panic. An FFI consumer will do exactly this on an early
    // error path.
    let cfg = Config {
        hostname: "test".into(),
        authkey: String::new(),
        state_dir: std::env::temp_dir().join("gf-native-test"),
        supports_h264: false,
        indices_raw: false,
        n_export_buffers: 3,
        preferred_modifiers: vec![],
        debug_map_frames: false,
    };
    match Client::new(cfg) {
        Ok(c) => drop(c),
        // No tailnet in a unit-test environment is fine; the point is that
        // the failure path is clean rather than a hang or a panic.
        Err(e) => eprintln!("client construction failed as expected offline: {e}"),
    }
}
