use ghostframe_client_net::{ClientNet, ClientNetConfig};

#[test]
fn new_client_has_nothing_to_transmit_before_connect() {
    let cfg = ClientNetConfig {
        server_name: "localhost".into(),
        server_cert_sha256: [0u8; 32],
        indices_raw_enabled: true,
        supports_h264: false,
    };
    let mut client = ClientNet::new(cfg, 0).expect("ClientNet::new");
    assert!(client.poll_transmit().is_none());
    assert!(!client.is_connected());
}
