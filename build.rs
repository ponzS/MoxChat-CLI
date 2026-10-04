use sha2::{Digest, Sha256};
use std::{env, fs, path::PathBuf};

fn main() {
    let target = env::var("TARGET").expect("Cargo target");
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let runtime = root.join("runtime").join(&target);
    let binary = runtime.join("mox-mls-runtime");
    let checksum = runtime.join("SHA256SUMS");
    println!("cargo:rerun-if-changed={}", binary.display());
    println!("cargo:rerun-if-changed={}", checksum.display());
    let bytes = fs::read(&binary).unwrap_or_else(|_| panic!("Missing bundled runtime for {target}. Check the supported platforms in README.md."));
    let expected = fs::read_to_string(&checksum).expect("MLS runtime SHA256SUMS");
    let hash = format!("{:x}", Sha256::digest(&bytes));
    assert_eq!(
        expected.split_whitespace().next(),
        Some(hash.as_str()),
        "MLS runtime checksum mismatch"
    );
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("mox-mls-runtime");
    fs::write(output, bytes).expect("Embed MLS runtime");
    println!("cargo:rustc-env=MOX_MLS_SHA256={hash}");
}
