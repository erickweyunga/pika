//! End-to-end tests of `pika run`: every `run/*.pk` program, and every package in
//! `run_packages/`, is run compiled to native code and interpreted. Both runs must produce
//! exactly the same standard output, standard error and exit status, which are compared with
//! the stored snapshot.
//!
//! The name of a program says how it ends: `ok_*` exits with 0, `panic_*` with 101,
//! `error_*` with 1 after an error that `main` raises, and `unsupported_*` is rejected with
//! status 1.

use std::path::Path;
use std::process::Command;

/// The observable behavior of `pika run target`, run in `dir`.
fn run(dir: &Path, target: &str, interpret: bool) -> (Option<i32>, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pika"));
    // Compiled programs must free all of their heap memory; a leak changes the exit status.
    command
        .current_dir(dir)
        .env("PIKA_LEAK_CHECK", "1")
        .arg("run")
        .arg(target);
    if interpret {
        command.arg("--interpret");
    }
    let output = command.output().expect("the pika binary runs");
    let text = format!(
        "exit: {}\n--- stdout ---\n{}--- stderr ---\n{}",
        output
            .status
            .code()
            .map_or("signal".to_owned(), |c| c.to_string()),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    (output.status.code(), text)
}

/// Runs the program `name`, `target` in `dir`, both ways, and checks that the runs agree and
/// end as its name says. Returns what the runs printed.
fn run_both(dir: &Path, target: &str, name: &str) -> String {
    let (status, native) = run(dir, target, false);
    let (_, interpreted) = run(dir, target, true);
    assert_eq!(
        native, interpreted,
        "native and interpreted runs differ for {name}"
    );
    let expected = match name.split('_').next() {
        Some("ok") => 0,
        Some("panic") => 101,
        Some("error" | "unsupported") => 1,
        _ => {
            panic!("{name}: test programs start with `ok_`, `panic_`, `error_` or `unsupported_`")
        }
    };
    assert_eq!(
        status,
        Some(expected),
        "{name} exited unexpectedly:\n{native}"
    );
    native
}

#[test]
fn run_programs() {
    insta::glob!("run/*.pk", |path| {
        let dir = path.parent().expect("test files are in a directory");
        let name = path.file_name().expect("file name").to_string_lossy();
        insta::assert_snapshot!(run_both(dir, &name, &name));
    });
}

#[test]
fn run_packages() {
    insta::glob!("run_packages/*/pika.toml", |manifest| {
        let dir = manifest.parent().expect("a package directory");
        let name = dir.file_name().expect("directory name").to_string_lossy();
        insta::assert_snapshot!(run_both(dir, ".", &name));
    });
}
