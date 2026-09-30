// Compiles src/hvf_tso.c into the dylib agentpc injects into QEMU (see qemu::hvf_tso).
use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=src/hvf_tso.c");
    // The compiler comes from $CC; a different one must rebuild the library.
    println!("cargo:rerun-if-env-changed=CC");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("hvf-tso.dylib");
    let cc = env::var("CC").unwrap_or_else(|_| "cc".into());
    let status = Command::new(&cc)
        .args([
            "-dynamiclib",
            "-O2",
            "-arch",
            "arm64",
            "-mmacosx-version-min=12.0",
        ])
        .args(["-framework", "Hypervisor", "-o"])
        .arg(&out)
        .arg("src/hvf_tso.c")
        .status()
        .unwrap_or_else(|e| panic!("run {cc}: {e}"));
    assert!(status.success(), "compiling src/hvf_tso.c failed");
}
