//! Cost of putting QUIC transmits on, and taking them off, the ghostbridge
//! socketpair, per packet versus batched. Measures the *framing and syscall*
//! pattern of `IoBridge::drain_outbound` / `process_inbound` in isolation
//! (blocking std `UnixStream` instead of tokio's, a reader thread draining the
//! far end), for a 4K full-screen frame worth of 1200-byte packets.
//!
//! ```text
//! cargo bench -p ghostframe-bench --bench frame_io
//! ```
//! Env: `FRAME_IO_ITERS` (default 30).

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::os::unix::net::UnixStream;
use std::time::Instant;

use ghostframe_lib::transport::ghostbridge::{
    encode_frame, encode_frame_append, encode_frame_into, parse_frame_rest,
};

const PKTS: usize = 8975;
const PKT_LEN: usize = 1200;
const CHUNKS: &[usize] = &[8 * 1024, 16 * 1024, 32 * 1024, 64 * 1024];

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn stats(name: &str, mut v: Vec<f64>) {
    let min = v.iter().cloned().fold(f64::MAX, f64::min);
    let max = v.iter().cloned().fold(0.0, f64::max);
    println!(
        "{name:<34} median {:>8.3} ms  min {min:>8.3}  max {max:>8.3}",
        median(&mut v)
    );
}

fn main() {
    let iters: usize = std::env::var("FRAME_IO_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let addr: SocketAddr = "100.64.0.7:41641".parse().unwrap();
    let payload = vec![0x5Au8; PKT_LEN];

    let (mut tx, mut rx) = UnixStream::pair().unwrap();
    let reader = std::thread::spawn(move || {
        let mut sink = vec![0u8; 1 << 20];
        let mut total = 0usize;
        loop {
            match rx.read(&mut sink) {
                Ok(0) | Err(_) => break total,
                Ok(n) => total += n,
            }
        }
    });

    let run = |tx: &mut UnixStream, f: &mut dyn FnMut(&mut UnixStream)| -> f64 {
        let t = Instant::now();
        f(tx);
        Instant::now().duration_since(t).as_secs_f64() * 1e3
    };

    let mut per_packet = vec![];
    let mut reused = vec![];
    let mut batched: Vec<Vec<f64>> = vec![];
    for round in 0..iters + 2 {
        let a = run(&mut tx, &mut |tx| {
            for _ in 0..PKTS {
                let f = encode_frame(&payload, &addr);
                tx.write_all(&f).unwrap();
            }
        });
        let b = run(&mut tx, &mut |tx| {
            let mut f = Vec::new();
            for _ in 0..PKTS {
                encode_frame_into(&mut f, &payload, &addr);
                tx.write_all(&f).unwrap();
            }
        });
        let mut cs = vec![];
        for &chunk in CHUNKS {
            cs.push(run(&mut tx, &mut |tx| {
                let mut batch = Vec::with_capacity(chunk + 2048);
                for _ in 0..PKTS {
                    encode_frame_append(&mut batch, &payload, &addr);
                    if batch.len() >= chunk {
                        tx.write_all(&batch).unwrap();
                        batch.clear();
                    }
                }
                tx.write_all(&batch).unwrap();
            }));
        }
        if round >= 2 {
            per_packet.push(a);
            reused.push(b);
            batched.push(cs);
        }
    }
    stats("outbound: encode_frame + write", per_packet);
    stats("outbound: encode_frame_into + write", reused);
    for (i, &chunk) in CHUNKS.iter().enumerate() {
        let v: Vec<f64> = batched.iter().map(|r| r[i]).collect();
        stats(&format!("outbound: batched {} KiB writes", chunk / 1024), v);
    }

    // Inbound: the parse half only (the read syscalls are the socket's).
    let frame = encode_frame(&payload[..80], &addr);
    let mut parse = vec![];
    for _ in 0..iters {
        let t = Instant::now();
        for _ in 0..PKTS {
            let rest = frame[8..].to_vec();
            let p = parse_frame_rest(&rest, 80).unwrap();
            std::hint::black_box(bytes::BytesMut::from(p.payload.as_slice()));
        }
        parse.push(Instant::now().duration_since(t).as_secs_f64() * 1e3);
    }
    stats("inbound: rest vec+parse+BytesMut", parse);
    drop(tx);
    let _ = reader.join();
}
