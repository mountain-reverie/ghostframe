// Replays a trace through TQUIC's delivery_rate.rs, compiled UNMODIFIED from src/delivery_rate.rs
// (fetched by the recipe in the vectors file header; it is gitignored). Prints each
// trace line, and after every A line the estimator's readings as an R line.
mod connection {
    pub mod space {
        use std::time::Instant;
        #[derive(Debug, Default, Clone, Copy)]
        pub struct RateSamplePacketState {
            pub delivered: u64,
            pub delivered_time: Option<Instant>,
            pub first_sent_time: Option<Instant>,
            pub is_app_limited: bool,
            pub tx_in_flight: u64,
            pub lost: u64,
        }
        #[derive(Debug, Clone)]
        pub struct SentPacket {
            pub pkt_num: u64,
            pub frames: Vec<u8>,
            pub time_sent: Instant,
            pub time_acked: Option<Instant>,
            pub time_lost: Option<Instant>,
            pub ack_eliciting: bool,
            pub in_flight: bool,
            pub has_data: bool,
            pub sent_size: usize,
            pub rate_sample_state: RateSamplePacketState,
        }
        impl Default for SentPacket {
            fn default() -> Self {
                SentPacket { pkt_num: 0, frames: vec![], time_sent: Instant::now(), time_acked: None, time_lost: None,
                    ack_eliciting: true, in_flight: true, has_data: false, sent_size: 0, rate_sample_state: Default::default() }
            }
        }
        pub struct AckedPacket;
    }
}
#[path = "delivery_rate.rs"]
mod delivery_rate;
use delivery_rate::DeliveryRateEstimator;
use connection::space::SentPacket;
use std::collections::HashMap;
use std::time::{Duration, Instant};

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let text = std::fs::read_to_string(path).unwrap();
    let base = Instant::now();
    let at = |us: u64| base + Duration::from_micros(us);
    let mut est = DeliveryRateEstimator::default();
    let mut pkts: HashMap<u64, SentPacket> = HashMap::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        match f[0] {
            "S" => {
                let t: u64 = f[1].parse().unwrap();
                let n: u64 = f[2].parse().unwrap();
                let size: usize = f[3].parse().unwrap();
                let infl: u64 = f[4].parse().unwrap();
                let lost: u64 = f[5].parse().unwrap();
                let mut p = SentPacket { pkt_num: n, time_sent: at(t), sent_size: size, ..SentPacket::default() };
                est.on_packet_sent(&mut p, infl, lost);
                pkts.insert(n, p);
                println!("{line}");
            }
            "L" => { est.set_app_limited(f[1] == "1"); println!("{line}"); }
            "A" => {
                let t: u64 = f[1].parse().unwrap();
                for n in f[2].split(',') {
                    let n: u64 = n.parse().unwrap();
                    let p = pkts.get_mut(&n).unwrap();
                    p.time_acked = Some(at(t));
                    est.update_rate_sample(p);
                }
                est.generate_rate_sample();
                println!("{line}");
                println!("R {} {} {} {} {} {}", est.delivery_rate(), est.delivered(), est.sample_delivered(),
                    est.sample_prior_delivered(), est.is_sample_app_limited() as u8, est.sample_rtt().as_micros());
            }
            _ => {}
        }
    }
}
