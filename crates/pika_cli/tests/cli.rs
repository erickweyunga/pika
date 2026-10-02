//! End-to-end tests of the `pika` binary.

use std::path::PathBuf;
use std::process::{Command, Output};

fn pika(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pika"))
        .args(args)
        .output()
        .expect("the pika binary runs")
}

/// Writes `contents` to a uniquely named file in the test scratch directory.
fn source_file(name: &str, contents: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, contents).expect("scratch directory is writable");
    path
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("output is UTF-8")
}

#[test]
fn check_accepts_a_valid_file() {
    let file = source_file("cli_valid.pk", ":put \"hello\"\n");
    let output = pika(&["check", file.to_str().unwrap()]);
    assert!(output.status.success(), "stderr: {}", text(&output.stderr));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn check_reports_syntax_errors() {
    let file = source_file("cli_invalid.pk", ":put $a + $b\n");
    let output = pika(&["check", file.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("[E0102]"), "stderr: {stderr}");
    assert!(stderr.contains("cli_invalid.pk:1:9"), "stderr: {stderr}");
}

#[test]
fn check_reports_type_errors() {
    let file = source_file("cli_type_error.pk", ":local x:i64 \"text\"\n:put $x\n");
    let output = pika(&["check", file.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("[E0301]"), "stderr: {stderr}");
}

#[test]
fn check_succeeds_with_only_warnings() {
    let file = source_file("cli_warning.pk", ":local x 1\n:put $x\n");
    let output = pika(&["check", file.to_str().unwrap()]);
    assert!(output.status.success());
    assert!(text(&output.stderr).contains("[W0001]"));
}

#[test]
fn types_prints_inferred_types() {
    let file = source_file("cli_types.pk", ":const x 1.5\n:put $x\n");
    let output = pika(&["types", file.to_str().unwrap()]);
    assert!(output.status.success());
    assert!(text(&output.stdout).contains("const x: f64"));
}

#[test]
fn check_fails_if_any_file_fails() {
    let valid = source_file("cli_many_valid.pk", ":put 1\n");
    let invalid = source_file("cli_many_invalid.pk", ":put (1\n");
    let output = pika(&["check", valid.to_str().unwrap(), invalid.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    assert!(text(&output.stderr).contains("[E0116]"));
}

#[test]
fn check_reports_unreadable_files() {
    let output = pika(&["check", "definitely/not/a/file.pk"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(text(&output.stderr).contains("cannot read definitely/not/a/file.pk"));
}

#[test]
fn parse_prints_the_syntax_tree() {
    let file = source_file("cli_parse.pk", ":put 1\n");
    let output = pika(&["parse", file.to_str().unwrap()]);
    assert!(output.status.success());
    let stdout = text(&output.stdout);
    assert!(stdout.starts_with("SourceFile@0..7"), "stdout: {stdout}");
    assert!(stdout.contains("CommandName@0..4"), "stdout: {stdout}");
}

#[test]
fn lex_prints_tokens() {
    let file = source_file("cli_lex.pk", ":put 1\n");
    let output = pika(&["lex", file.to_str().unwrap()]);
    assert!(output.status.success());
    assert!(text(&output.stdout).contains("+Ident 1..4 \"put\""));
}

/// Runs `pika run` on `file` with `args` after `--`, writing `input` to its standard input.
fn run_with_input(file: &str, args: &[&str], input: &str, interpret: bool) -> Output {
    use std::io::Write;
    let mut command = Command::new(env!("CARGO_BIN_EXE_pika"));
    command.arg("run").arg(file);
    if interpret {
        command.arg("--interpret");
    }
    let mut child = command
        .arg("--")
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the pika binary runs");
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(input.as_bytes())
        .expect("the program reads its input");
    child.wait_with_output().expect("the program finishes")
}

#[test]
fn run_passes_arguments_and_input_to_the_program() {
    let file = source_file(
        "cli_args_input.pk",
        ":use /std/env\n:use /std/io\n:fn main do={\n:put [/env/args]\n/io/print \"name? \"\n:const name [/io/read_line]\n:put \"hi $name\"\n:put [/io/read_line]\n:put [/io/read_line]\n}\n",
    );
    for interpret in [false, true] {
        let output = run_with_input(
            file.to_str().unwrap(),
            &["a", "b c"],
            "Ada\r\nlast",
            interpret,
        );
        assert!(output.status.success(), "stderr: {}", text(&output.stderr));
        assert_eq!(
            text(&output.stdout),
            "{\"a\"; \"b c\"}\nname? hi [some \"Ada\"]\n[some \"last\"]\nnone\n"
        );
    }
}
