//! Generator for the unrolled lifting functions of `cdf53_encode.comp`.
//!
//! The shader holds each line of a 5/3 lifting pass in locals (`v0..v31`),
//! which is what makes one-invocation-per-line fast, and is too long and
//! too regular to maintain by hand. This module writes that block; the
//! shader carries it between the BEGIN/END markers below, checked in, so
//! the build never runs a generator. `generated_lifting_matches_the_checked_in_shader`
//! fails when the two drift; `just shaders-cdf53` regenerates the block
//! (via the ignored `regenerate` test) and recompiles the `.spv`.
//!
//! The lifting is `cdf53::row_forward` (ghostframe-protocol) unrolled:
//! predict odd samples, update even ones, mirror at both ends, then
//! deinterleave (low half first). Rows read consecutive words; columns read
//! every `ROW`-th word of the padded work area.

use std::path::PathBuf;

/// Padded row stride of the shader's work area, in words. The shader
/// declares the same constant; the staleness test checks it.
const ROW: usize = 33;
const SHADER: &str = "src/capture/shaders/cdf53_encode.comp";
const BEGIN: &str = "// BEGIN GENERATED LIFTING (cdf53_shader_gen.rs; `just shaders-cdf53`)\n";
const END: &str = "// END GENERATED LIFTING\n";

/// `lift_rows_{n}` and `lift_cols_{n}` for a line of `n` samples starting
/// at word `base` of the shared array `A`.
fn gen_lift(n: usize) -> String {
    let mut s = String::new();
    for (name, stride) in [("rows", 1usize), ("cols", ROW)] {
        s += &format!("void lift_{name}_{n}(uint base) {{\n");
        for i in 0..n {
            s += &format!("    int v{i} = A[base + {}u];\n", i * stride);
        }
        // Predict: odd samples become high-pass. Right neighbour mirrors.
        for i in (1..n).step_by(2) {
            let r = if i + 1 < n { i + 1 } else { i - 1 };
            s += &format!("    v{i} -= (v{} + v{r}) >> 1;\n", i - 1);
        }
        // Update: even samples become low-pass. Left neighbour mirrors.
        for i in (0..n).step_by(2) {
            let l = if i == 0 { 1 } else { i - 1 };
            s += &format!("    v{i} += (v{l} + v{} + 2) >> 2;\n", i + 1);
        }
        // Deinterleave: L0 L1 .. then H0 H1 ..
        for j in 0..n / 2 {
            s += &format!("    A[base + {}u] = v{};\n", j * stride, 2 * j);
            s += &format!("    A[base + {}u] = v{};\n", (j + n / 2) * stride, 2 * j + 1);
        }
        s += "}\n";
    }
    s
}

fn generated() -> String {
    [32, 16, 8].iter().map(|&n| gen_lift(n)).collect()
}

fn shader_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(SHADER)
}

/// `src` with the text between the markers replaced by `body`.
fn splice(src: &str, body: &str) -> String {
    let b = src.find(BEGIN).expect("BEGIN GENERATED LIFTING marker") + BEGIN.len();
    let e = src.find(END).expect("END GENERATED LIFTING marker");
    assert!(b <= e, "lifting markers out of order");
    format!("{}{}{}", &src[..b], body, &src[e..])
}

#[test]
fn generated_lifting_matches_the_checked_in_shader() {
    let src = std::fs::read_to_string(shader_path()).expect("read cdf53_encode.comp");
    assert!(
        src.contains(&format!("const uint ROW = {ROW}u;")),
        "the shader's ROW must equal the generator's ({ROW})"
    );
    assert!(
        src == splice(&src, &generated()),
        "cdf53_encode.comp's lifting block is stale: run `just shaders-cdf53`"
    );
}

/// Rewrites the shader's lifting block. Not a check: `just shaders-cdf53`
/// runs it, then compiles the `.spv`.
#[test]
#[ignore = "rewrites the checked-in shader; run through `just shaders-cdf53`"]
fn regenerate() {
    let p = shader_path();
    let src = std::fs::read_to_string(&p).expect("read cdf53_encode.comp");
    std::fs::write(&p, splice(&src, &generated())).expect("write cdf53_encode.comp");
}
