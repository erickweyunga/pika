//! Snapshot tests of semantic analysis: every `check/*.pk` file, and every package in
//! `check_packages/`, is analyzed, and the inferred types and rendered diagnostics are compared
//! with the stored snapshot. Programs named `ok_*` must have no errors (warnings are allowed);
//! programs named `err_*` must have at least one.

use pika_diagnostics::{RenderOptions, render_map};
use pika_driver::{Analysis, Sources};

#[test]
fn check_snapshots() {
    insta::glob!("check/*.pk", |path| {
        let source = std::fs::read_to_string(path).expect("readable test input");
        let name = path.file_name().expect("file name").to_string_lossy();
        let analysis = pika_driver::check(Sources::single_file(name.as_ref(), source));
        insta::assert_snapshot!(snapshot(&name, &analysis));
    });
}

#[test]
fn package_snapshots() {
    insta::glob!("check_packages/*/pika.toml", |manifest| {
        let dir = manifest.parent().expect("a package directory");
        let name = dir.file_name().expect("directory name").to_string_lossy();
        let sources = pika_driver::load_package(dir).expect("a loadable package");
        let analysis = pika_driver::check(sources);
        // File names are shown relative to the package.
        let prefix = format!("{}/", dir.display());
        insta::assert_snapshot!(snapshot(&name, &analysis).replace(&prefix, ""));
    });
}

/// The types and diagnostics of the program `name`, checking that its name matches whether it
/// has errors.
fn snapshot(name: &str, analysis: &Analysis) -> String {
    let rendered = render_map(
        &analysis.diagnostics,
        &analysis.sources.map,
        RenderOptions::default(),
    );
    if name.starts_with("ok_") {
        assert!(
            !analysis.has_errors(),
            "{name} should have no errors:\n{rendered}"
        );
    } else {
        assert!(
            name.starts_with("err_"),
            "test programs must be named `ok_*` or `err_*`"
        );
        assert!(analysis.has_errors(), "{name} should have errors");
    }
    let mut snapshot = pika_driver::describe_types(analysis);
    if !analysis.diagnostics.is_empty() {
        snapshot.push_str("\n--- diagnostics ---\n");
        snapshot.push_str(&rendered);
    }
    snapshot
}
