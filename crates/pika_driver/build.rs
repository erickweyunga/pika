//! Embeds the standard library: generates `std_files.rs`, the path and text of every `.pk`
//! file in the repository's `std` directory.

use std::fmt::Write;
use std::path::{Path, PathBuf};

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let std_dir = manifest.join("../../std");
    println!("cargo::rerun-if-changed={}", std_dir.display());
    let mut files = Vec::new();
    find(&std_dir, &mut files);
    files.sort();
    let mut generated = String::from("&[\n");
    for file in &files {
        println!("cargo::rerun-if-changed={}", file.display());
        let relative = file
            .strip_prefix(&std_dir)
            .expect("found in the std directory")
            .to_string_lossy()
            .replace('\\', "/");
        let absolute = file.canonicalize().expect("an existing file");
        writeln!(
            generated,
            "    ({relative:?}, include_str!({:?})),",
            absolute.display().to_string()
        )
        .expect("writing to a String");
    }
    generated.push_str("]\n");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("set by cargo"));
    std::fs::write(out.join("std_files.rs"), generated).expect("writable output directory");
}

/// Adds the `.pk` files under `dir` to `files`, recursively.
fn find(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            find(&path, files);
        } else if path.extension().is_some_and(|e| e == "pk") {
            files.push(path);
        }
    }
}
