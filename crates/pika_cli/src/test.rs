//! `pika test`: runs the tests of a program, each in a process of its own, so that a test
//! that panics does not stop the others.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output, Stdio};
use std::sync::Mutex;
use std::sync::mpsc;

use crate::analyze;

/// What `pika test` was asked to do.
pub(crate) struct TestOptions {
    /// The program: a `.pk` file, or a package directory.
    pub(crate) path: PathBuf,
    /// Only the tests whose names contain this text.
    pub(crate) filter: Option<String>,
    /// Run with the MIR interpreter instead of compiling to native code.
    pub(crate) interpret: bool,
    /// Run only the test at this position, in this process: what each worker process does.
    pub(crate) run_test: Option<usize>,
}

/// The outcome of one test.
enum Outcome {
    Passed,
    /// The test failed; what it wrote, and why it failed.
    Failed {
        output: String,
        reason: String,
    },
}

/// Runs the tests that `options` select, or one test in this process.
pub(crate) fn test(options: &TestOptions) -> ExitCode {
    if let Some(index) = options.run_test {
        return run_one(&options.path, index, options.interpret);
    }
    let Some(analysis) = analyze(&options.path, true) else {
        return ExitCode::FAILURE;
    };
    if let Err(error) = pika_driver::runnable(&analysis) {
        return run_error(error, &analysis);
    }
    let names = pika_driver::test_names(&analysis);
    let selected: Vec<usize> = names
        .iter()
        .enumerate()
        .filter(|(_, name)| {
            options
                .filter
                .as_ref()
                .is_none_or(|f| name.contains(f.as_str()))
        })
        .map(|(index, _)| index)
        .collect();
    let filtered_out = names.len() - selected.len();
    println!(
        "running {} test{}",
        selected.len(),
        if selected.len() == 1 { "" } else { "s" }
    );
    let mut failures: Vec<(String, String, String)> = Vec::new();
    run_all(options, &selected, |index, outcome| {
        let name = &names[index];
        match outcome {
            Outcome::Passed => println!("test {name} ... ok"),
            Outcome::Failed { output, reason } => {
                println!("test {name} ... FAILED");
                failures.push((name.clone(), output, reason));
            }
        }
    });
    let failed = failures.len();
    if !failures.is_empty() {
        println!("\nfailures:");
        for (name, output, reason) in &failures {
            println!("\n---- {name} ----\n{output}{reason}");
        }
    }
    let passed = selected.len() - failed;
    println!(
        "\ntest result: {}. {passed} passed; {failed} failed; {filtered_out} filtered out",
        if failed == 0 { "ok" } else { "FAILED" }
    );
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Reports why a program cannot run: its errors were reported already, and features that
/// cannot run yet are reported now.
fn run_error(error: pika_driver::RunError, analysis: &pika_driver::Analysis) -> ExitCode {
    if let pika_driver::RunError::Unsupported(diagnostics) = error {
        crate::report(&diagnostics, &analysis.sources);
    }
    ExitCode::FAILURE
}

/// Runs the test at `index` in this process; its exit status is the test's.
fn run_one(path: &Path, index: usize, interpret: bool) -> ExitCode {
    let Some(analysis) = analyze(path, false) else {
        return ExitCode::FAILURE;
    };
    let program = match pika_driver::runnable(&analysis) {
        Ok(program) => program,
        Err(error) => return run_error(error, &analysis),
    };
    if index >= program.tests.len() {
        eprintln!("error: there is no test at position {index}");
        return ExitCode::FAILURE;
    }
    let test = pika_driver::test_program(program, index);
    let status = if interpret {
        let mut out = std::io::stdout().lock();
        let mut err = std::io::stderr().lock();
        pika_driver::interpret(&test, &analysis.sources.map, Vec::new(), &mut out, &mut err)
    } else {
        pika_driver::run_native(&test, &analysis.sources.map, Vec::new())
    };
    ExitCode::from(u8::try_from(status).unwrap_or(1))
}

/// Runs the tests at `selected` in worker processes, several at a time, and gives each outcome
/// to `report` in the order of `selected`.
fn run_all(options: &TestOptions, selected: &[usize], mut report: impl FnMut(usize, Outcome)) {
    let workers = std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(selected.len())
        .max(1);
    let queue = Mutex::new(
        selected
            .iter()
            .copied()
            .enumerate()
            .collect::<VecDeque<_>>(),
    );
    let (sender, receiver) = mpsc::channel();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let sender = sender.clone();
            let queue = &queue;
            scope.spawn(move || {
                loop {
                    let next = queue
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .pop_front();
                    let Some((position, index)) = next else {
                        break;
                    };
                    let outcome = run_worker(options, index);
                    if sender.send((position, index, outcome)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(sender);
        // Outcomes arrive in any order; they are reported in order.
        let mut pending: Vec<Option<(usize, Outcome)>> = selected.iter().map(|_| None).collect();
        let mut next = 0;
        for (position, index, outcome) in receiver {
            pending[position] = Some((index, outcome));
            while let Some(Some((index, outcome))) = pending.get_mut(next).map(Option::take) {
                report(index, outcome);
                next += 1;
            }
        }
    });
}

/// Runs the test at `index` in a new process of this program.
fn run_worker(options: &TestOptions, index: usize) -> Outcome {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            return Outcome::Failed {
                output: String::new(),
                reason: format!("cannot start the test: {error}"),
            };
        }
    };
    let mut command = Command::new(exe);
    command
        .arg("test")
        .arg(&options.path)
        .arg("--run-test")
        .arg(index.to_string())
        .stdin(Stdio::null());
    if options.interpret {
        command.arg("--interpret");
    }
    match command.output() {
        Ok(output) => outcome_of(&output),
        Err(error) => Outcome::Failed {
            output: String::new(),
            reason: format!("cannot start the test: {error}"),
        },
    }
}

/// The outcome of a test from what its process did.
fn outcome_of(output: &Output) -> Outcome {
    if output.status.success() {
        return Outcome::Passed;
    }
    let mut text = String::new();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stdout.is_empty() {
        let _ = writeln!(text, "--- stdout ---\n{}", stdout.trim_end());
    }
    if !stderr.is_empty() {
        let _ = writeln!(text, "--- stderr ---\n{}", stderr.trim_end());
    }
    let reason = match output.status.code() {
        Some(101) => "the test panicked".to_owned(),
        Some(1) => "the test raised an error".to_owned(),
        Some(102) => "the test did not free all of its memory".to_owned(),
        Some(code) => format!("the test exited with status {code}"),
        None => "the test was stopped by a signal".to_owned(),
    };
    Outcome::Failed {
        output: text,
        reason,
    }
}
