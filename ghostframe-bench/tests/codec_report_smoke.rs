//! Smoke test for the M3.5 codec_report binary.
//!
//! Runs the binary with --scenes solid --scene-duration 1s and asserts
//! the output file exists, parses as a markdown document, and contains
//! the expected section headers.

use std::process::Command;

#[test]
#[ignore = "requires Docker + GPU; run with --ignored"]
fn codec_report_smoke() {
    let out = std::env::temp_dir().join("codec_report_smoke.md");
    let _ = std::fs::remove_file(&out);

    let status = Command::new(env!("CARGO_BIN_EXE_codec_report"))
        .args([
            "--scenes",
            "solid",
            "--scene-duration",
            "1s",
            "--output",
            out.to_str().unwrap(),
        ])
        .status()
        .expect("spawn codec_report");
    assert!(status.success(), "codec_report exited with {status:?}");

    let text = std::fs::read_to_string(&out).expect("output file exists");
    assert!(
        text.contains("# M3 Codec Bench Results"),
        "preamble header missing"
    );
    assert!(
        text.contains("## 2. End-to-end latency per scene"),
        "section 2 header missing"
    );
    assert!(
        text.contains("## 3. Wire bandwidth"),
        "section 3 header missing"
    );
    assert!(
        text.contains("## 4. Per-codec micro-bench"),
        "section 4 header missing"
    );
    assert!(
        text.contains("## 5. Per-codec compressed size"),
        "section 5 header missing"
    );
    assert!(
        text.contains("## 6. Cdf53 SSIM vs passes"),
        "section 6 header missing"
    );
    assert!(
        text.contains("## 7. BC1 (removed)"),
        "section 7 header missing"
    );
    assert!(
        text.contains("## 8. Lossless-strategy recommendation"),
        "section 8 header missing"
    );
    assert!(
        text.contains("## 9. Mode dwell × bandwidth × scene"),
        "section 9 header missing"
    );
    assert!(
        text.contains("## 10. Override-trigger frequency"),
        "section 10 header missing"
    );
    assert!(
        text.contains("`solid`"),
        "solid scene row missing from section 2"
    );
}
