#![no_main]
use ghostframe_client_net::{ClientNet, ClientNetConfig};
use libfuzzer_sys::fuzz_target;
use std::net::{Ipv6Addr, SocketAddr};

fuzz_target!(|data: &[u8]| {
    let cfg = ClientNetConfig {
        server_name: "localhost".into(),
        server_cert_sha256: [0u8; 32],
        indices_raw_enabled: true,
        supports_h264: false,
    };
    let mut client = ClientNet::new(cfg, 0).expect("new");
    let from = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 443);
    // Must never panic, whatever the bytes are.
    client.handle_udp(data, from, 0);
});
