//! The advertised capability is the AND of what the host asked for and what
//! the machine can do.

use ghostframe_client_native::{Client, Config};

fn config(supports_h264: bool) -> Config {
    Config {
        hostname: "cap-test".into(),
        authkey: String::new(),
        state_dir: std::env::temp_dir().join("ghostframe-cap-test"),
        supports_h264,
        indices_raw: false,
        n_export_buffers: 3,
        preferred_modifiers: vec![],
        debug_map_frames: false,
        display: None,
    }
}

/// A host that does not want H.264 never gets it, regardless of hardware.
#[test]
fn opting_out_is_absolute() {
    let client = Client::new(config(false)).expect("create");
    assert!(!client.supports_h264());
}

/// A host that asks for H.264 gets it only where the hardware agrees. On a
/// machine with VA-API this is `true`; on one without, `false` -- and the
/// client is still perfectly usable, which is the point.
#[test]
fn opting_in_follows_the_probe() {
    let client = Client::new(config(true)).expect("create");
    assert_eq!(
        client.supports_h264(),
        ghostframe_client_h264::vaapi_h264_decode_available(),
        "the effective capability must equal the probe when the host opts in"
    );
}
