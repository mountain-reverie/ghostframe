use ghostframe_client_core::{ClientConfig, ClientCore, PollOutput};

#[test]
fn new_core_emits_hello_on_stream_then_nothing() {
    let mut core = ClientCore::new(
        ClientConfig {
            indices_raw_enabled: true,
            supports_h264: true,
            ..Default::default()
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
            ..Default::default()
        },
        0,
    );
    assert!(core.handle_datagram(&[], 0).is_empty());
}

#[test]
fn hello_advertises_the_v2_wire_only_when_configured() {
    let mut core = ClientCore::new(
        ClientConfig {
            indices_raw_enabled: true,
            supports_h264: true,
            h264_acked_ref: true,
            ..Default::default()
        },
        0,
    );
    assert_eq!(
        core.poll_transmit(0),
        Some(PollOutput::Stream(vec![0x03, 0x07]))
    );
}

#[test]
fn request_reset_queues_a_0x09_stream_message() {
    use ghostframe_client_core::ResetReason;
    let mut core = ClientCore::new(ClientConfig::default(), 0);
    let _hello = core.poll_transmit(0);
    core.request_reset(ResetReason::DecoderFailure, 9);
    assert_eq!(
        core.poll_transmit(0),
        Some(PollOutput::Stream(vec![0x09, 0x01, 9, 0, 0, 0]))
    );
    assert_eq!(core.poll_transmit(0), None);
}
