use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let config = cbindgen::Config::from_root_or_default(&manifest_dir);
    let include_dir = PathBuf::from(&manifest_dir).join("include");
    std::fs::create_dir_all(&include_dir).expect("create include/");
    cbindgen::Builder::new()
        .with_config(config)
        .with_crate(manifest_dir.clone())
        .with_language(cbindgen::Language::C)
        .generate()
        .expect("cbindgen failed")
        .write_to_file(include_dir.join("ghostframe_client.h"));
    println!("cargo:rerun-if-changed=src");
}
