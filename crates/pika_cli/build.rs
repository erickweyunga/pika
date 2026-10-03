//! Builds the runtime library that `pika build` links into executables: `pika_runtime`,
//! compiled with optimizations as a static library for the target of this build, into
//! `OUT_DIR`. The `pika` executable carries it, with the system libraries it needs, so that
//! the executables it makes always use its own runtime.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let root = manifest.join("../..");
    let target = std::env::var("TARGET").expect("cargo sets TARGET");
    // For finding the Visual C++ linker for this target.
    println!("cargo::rustc-env=PIKA_TARGET={target}");
    for path in ["crates/pika_runtime", "Cargo.toml", "Cargo.lock"] {
        println!("cargo::rerun-if-changed={}", root.join(path).display());
    }

    let target_dir = out.join("runtime");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .arg("rustc")
        .arg("--manifest-path")
        .arg(root.join("Cargo.toml"))
        .args([
            "--package",
            "pika_runtime",
            "--lib",
            "--release",
            "--locked",
        ])
        .args(["--crate-type", "staticlib", "--target", &target])
        .arg("--target-dir")
        .arg(&target_dir)
        .args(["--", "--print", "native-static-libs"])
        // A linter wrapping the compiler for this build has nothing to do with the library.
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .output()
        .expect("cargo can be started");
    let messages = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "building the runtime library failed:\n{messages}"
    );
    // Rust reports the system libraries a static library needs, in the form of linker
    // arguments, once, when it builds it; they are kept for linking executables.
    let native = messages
        .lines()
        .find_map(|line| line.split("native-static-libs:").nth(1))
        .map(str::trim);
    let native_file = out.join("native-static-libs.txt");
    match native {
        Some(native) => std::fs::write(&native_file, native).expect("OUT_DIR is writable"),
        // Up to date: the libraries were recorded when it was built.
        None => assert!(
            native_file.exists(),
            "the runtime library was built without reporting its system libraries"
        ),
    }

    let name = if target.contains("windows-msvc") {
        "pika_runtime.lib"
    } else {
        "libpika_runtime.a"
    };
    copy(
        &target_dir.join(&target).join("release").join(name),
        &out.join("pika_runtime.lib.bin"),
    );
}

fn copy(from: &Path, to: &Path) {
    std::fs::copy(from, to)
        .unwrap_or_else(|error| panic!("cannot copy {}: {error}", from.display()));
}
