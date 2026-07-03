use ghostframe_client_core::{ClientConfig, ClientCore, PollOutput};

#[test]
fn new_core_emits_hello_on_stream_then_nothing() {
    let mut core = ClientCore::new(
        ClientConfig {
            indices_raw_enabled: true,
            supports_h264: true,
        },
        0,
    );
    // Hello: [0x03, caps] with bit0=indicesRaw, bit1=h264 (feedback.ts:81-86)
    assert_eq!(
        core.poll_transmit(0),
        Some(PollOutput::Stream(vec![0x03, 0x03]))
    );
    assert_eq!(core.poll_transmit(0), None);
}

#[test]
fn empty_datagram_is_ignored() {
    let mut core = ClientCore::new(
        ClientConfig {
            indices_raw_enabled: false,
            supports_h264: false,
        },
        0,
    );
    assert!(core.handle_datagram(&[], 0).is_empty());
}
