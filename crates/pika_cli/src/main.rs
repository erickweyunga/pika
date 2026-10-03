//! The `pika` command-line tool.

mod bundle;
mod fmt;
mod test;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use pika_diagnostics::{Diagnostic, RenderOptions, SourceMap, render_map};
use pika_driver::Sources;

#[derive(Parser)]
#[command(name = "pika", version, about = "The Pika programming language")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check programs for errors without building them.
    Check {
        /// The programs to check: `.pk` files, or package directories with a `pika.toml`.
        #[arg(default_value = ".")]
        paths: Vec<PathBuf>,
    },
    /// Compile and run a program.
    Run {
        /// The program: a `.pk` file, or a package directory with a `pika.toml`.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Run with the MIR interpreter instead of compiling to native code.
        #[arg(long)]
        interpret: bool,
        /// Arguments for the program, after `--`.
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Build a program into an executable, which runs it with nothing else installed.
    Build {
        /// The program: a `.pk` file, or a package directory with a `pika.toml`.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Where to write the executable; by default, in the current directory, named after
        /// the package or the file.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Rewrite source files in the canonical layout.
    Fmt {
        /// `.pk` files, or directories whose `.pk` files to format.
        #[arg(default_value = ".")]
        paths: Vec<PathBuf>,
        /// Change nothing: list the files that are not formatted, and fail if there are any.
        #[arg(long)]
        check: bool,
    },
    /// Run the tests of a program, each in a process of its own.
    Test {
        /// The program: a `.pk` file, or a package directory with a `pika.toml`.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Only run the tests whose names contain this text.
        filter: Option<String>,
        /// Run with the MIR interpreter instead of compiling to native code.
        #[arg(long)]
        interpret: bool,
        /// Run only the test at this position, in this process (what `pika test` runs in each
        /// of its processes).
        #[arg(long, hide = true)]
        run_test: Option<usize>,
    },
    /// Print the tokens of a source file (a compiler development tool).
    Lex {
        /// The `.pk` file to lex.
        file: PathBuf,
        /// Also print whitespace and comments.
        #[arg(long)]
        trivia: bool,
    },
    /// Print the syntax tree of a source file (a compiler development tool).
    Parse {
        /// The `.pk` file to parse.
        file: PathBuf,
    },
    /// Print the inferred types of a program (a compiler development tool).
    Types {
        /// The program: a `.pk` file, or a package directory with a `pika.toml`.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

fn main() -> ExitCode {
    // An executable built by `pika build` runs the program it carries.
    if let Some(status) = bundle::run_embedded() {
        return status;
    }
    match Cli::parse().command {
        Command::Check { paths } => check(&paths),
        Command::Run {
            path,
            interpret,
            args,
        } => run(&path, interpret, args),
        Command::Build { path, output } => bundle::build(&path, output.as_deref()),
        Command::Fmt { paths, check } => fmt::fmt(&paths, check),
        Command::Test {
            path,
            filter,
            interpret,
            run_test,
        } => test::test(&test::TestOptions {
            path,
            filter,
            interpret,
            run_test,
        }),
        Command::Lex { file, trivia } => with_source(&file, |source| {
            let lexed = pika_syntax::lex(source);
            print!(
                "{}",
                pika_syntax::dump_tokens(source, &lexed.tokens, trivia)
            );
            lexed.diagnostics
        }),
        Command::Parse { file } => with_source(&file, |source| {
            let parse = pika_syntax::parse(source);
            print!("{}", parse.debug_tree());
            parse.diagnostics().to_vec()
        }),
        Command::Types { path } => {
            let Some(analysis) = analyze(&path, true) else {
                return ExitCode::FAILURE;
            };
            print!("{}", pika_driver::describe_types(&analysis));
            exit_status(&analysis.diagnostics)
        }
    }
}

/// Loads the program at `path`, a source file or a package directory, and analyzes it,
/// reporting its diagnostics on stderr if `report_diagnostics`. Returns `None` if it cannot
/// be loaded.
fn analyze(path: &Path, report_diagnostics: bool) -> Option<pika_driver::Analysis> {
    let sources = if path.is_dir() {
        pika_driver::load_package(path).map_err(|error| error.to_string())
    } else {
        std::fs::read_to_string(path)
            .map(|text| Sources::single_file(path.display().to_string(), text))
            .map_err(|error| format!("cannot read {}: {error}", path.display()))
    };
    let sources = match sources {
        Ok(sources) => sources,
        Err(message) => {
            eprintln!("error: {message}");
            return None;
        }
    };
    let analysis = pika_driver::check(sources);
    if report_diagnostics {
        report(&analysis.diagnostics, &analysis.sources);
    }
    Some(analysis)
}

/// Prints `diagnostics` about the files of `sources` on stderr.
fn report(diagnostics: &[Diagnostic], sources: &Sources) {
    if diagnostics.is_empty() {
        return;
    }
    let options = RenderOptions {
        color: std::io::stderr().is_terminal(),
    };
    eprint!("{}", render_map(diagnostics, &sources.map, options));
}

/// Failure if any of `diagnostics` is an error.
fn exit_status(diagnostics: &[Diagnostic]) -> ExitCode {
    if diagnostics.iter().any(Diagnostic::is_error) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn check(paths: &[PathBuf]) -> ExitCode {
    let mut failed = false;
    for path in paths {
        match analyze(path, true) {
            Some(analysis) if !analysis.has_errors() => {}
            _ => failed = true,
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn run(path: &Path, interpret: bool, args: Vec<String>) -> ExitCode {
    let Some(analysis) = analyze(path, true) else {
        return ExitCode::FAILURE;
    };
    if !analysis.sources.root_package().binary {
        eprintln!(
            "error: {} is a library, not a program: a program has src/main.pk",
            path.display()
        );
        return ExitCode::FAILURE;
    }
    let program = match pika_driver::runnable(&analysis) {
        Ok(program) => program,
        Err(pika_driver::RunError::InvalidProgram) => return ExitCode::FAILURE,
        Err(pika_driver::RunError::Unsupported(diagnostics)) => {
            report(&diagnostics, &analysis.sources);
            return ExitCode::FAILURE;
        }
    };
    let status = if interpret {
        let mut out = std::io::stdout().lock();
        let mut err = std::io::stderr().lock();
        pika_driver::interpret(program, &analysis.sources.map, args, &mut out, &mut err)
    } else {
        pika_driver::run_native(program, &analysis.sources.map, args)
    };
    ExitCode::from(u8::try_from(status).unwrap_or(1))
}

/// Renders diagnostics about one source file, named `name`, whose text is `source`.
pub(crate) fn render_file(
    diagnostics: &[Diagnostic],
    name: &str,
    source: &str,
    options: RenderOptions,
) -> String {
    let mut map = SourceMap::default();
    pika_syntax::add_file(&mut map, name, source);
    render_map(diagnostics, &map, options)
}

/// Reads `file`, runs `f` on its contents, and reports the returned diagnostics on stderr.
/// Fails if the file cannot be read or any diagnostic is an error.
fn with_source(file: &Path, f: impl FnOnce(&str) -> Vec<Diagnostic>) -> ExitCode {
    let source = match std::fs::read_to_string(file) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("error: cannot read {}: {error}", file.display());
            return ExitCode::FAILURE;
        }
    };
    let diagnostics = f(&source);
    if diagnostics.is_empty() {
        return ExitCode::SUCCESS;
    }
    let options = RenderOptions {
        color: std::io::stderr().is_terminal(),
    };
    eprint!(
        "{}",
        render_file(&diagnostics, &file.display().to_string(), &source, options)
    );
    if diagnostics.iter().any(Diagnostic::is_error) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
