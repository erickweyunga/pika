//! `pika fmt`: rewrites source files in the canonical layout.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use pika_diagnostics::{Diagnostic, RenderOptions, render};
use pika_fmt::FormatError;

/// Formats the `.pk` files at `paths`, which are files or directories searched recursively.
/// With `check`, changes nothing: lists the files that are not formatted, and fails if any.
pub(crate) fn fmt(paths: &[PathBuf], check: bool) -> ExitCode {
    let mut files = Vec::new();
    for path in paths {
        if path.is_dir() {
            find_sources(path, &mut files);
        } else {
            files.push(path.clone());
        }
    }
    files.sort();
    let mut failed = false;
    for file in &files {
        if !fmt_file(file, check) {
            failed = true;
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Formats one file; returns false if it could not be formatted, or with `check`, if it is not
/// formatted.
fn fmt_file(file: &Path, check: bool) -> bool {
    let name = file.display().to_string();
    let source = match std::fs::read_to_string(file) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("error: cannot read {name}: {error}");
            return false;
        }
    };
    let formatted = match pika_fmt::format(&source) {
        Ok(formatted) => formatted,
        Err(FormatError::Syntax(diagnostics)) => {
            report(&diagnostics, &name, &source);
            eprintln!("error: {name} has syntax errors, so it was not formatted");
            return false;
        }
        Err(FormatError::Changed) => {
            eprintln!(
                "error: formatting {name} would change its meaning, so it was left as it is (this is a bug of the formatter)"
            );
            return false;
        }
    };
    if formatted == source {
        return true;
    }
    if check {
        println!("{name}");
        return false;
    }
    if let Err(error) = std::fs::write(file, formatted) {
        eprintln!("error: cannot write {name}: {error}");
        return false;
    }
    true
}

fn report(diagnostics: &[Diagnostic], name: &str, source: &str) {
    let options = RenderOptions {
        color: std::io::IsTerminal::is_terminal(&std::io::stderr()),
    };
    eprint!("{}", render(diagnostics, name, source, options));
}

/// Adds the `.pk` files under `dir` to `files`, recursively, skipping hidden directories and
/// build output (`target`).
fn find_sources(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        eprintln!("error: cannot read the directory {}", dir.display());
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if !name.starts_with('.') && name != "target" {
                find_sources(&path, files);
            }
        } else if path.extension().is_some_and(|e| e == "pk") {
            files.push(path);
        }
    }
}
