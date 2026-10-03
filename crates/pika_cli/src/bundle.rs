//! `pika build`: executables that carry a program, and running the program that an
//! executable carries.
//!
//! An executable built by `pika build` is a copy of this program followed by the sources of
//! the program it carries, their length as a little-endian `u64`, and [`MAGIC`]. The standard
//! library is not copied: every copy of this program has it. When it starts, an executable
//! that carries a program compiles and runs it, with all of its arguments.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use pika_driver::Sources;

/// The last bytes of an executable that carries a program.
const MAGIC: &[u8; 8] = b"PIKAPROG";

/// The exit status when the program an executable carries cannot run, which is a bug.
const INTERNAL_ERROR: u8 = 70;

/// If this executable carries a program, runs it with the process's arguments and returns
/// its exit status.
pub(crate) fn run_embedded() -> Option<ExitCode> {
    let exe = std::env::current_exe().ok()?;
    let bundle = read_bundle(&exe)?;
    let sources = match Sources::from_bundle(&bundle) {
        Ok(sources) => sources,
        Err(error) => {
            eprintln!("internal error: {error}");
            return Some(ExitCode::from(INTERNAL_ERROR));
        }
    };
    let analysis = pika_driver::check(sources);
    let Ok(program) = pika_driver::runnable(&analysis) else {
        crate::report(&analysis.diagnostics, &analysis.sources);
        eprintln!("internal error: the program in this executable does not compile");
        return Some(ExitCode::from(INTERNAL_ERROR));
    };
    let args = std::env::args().skip(1).collect();
    let status = pika_driver::run_native(program, &analysis.sources.map, args);
    Some(ExitCode::from(u8::try_from(status).unwrap_or(1)))
}

/// The sources that the executable at `exe` carries, if it carries a program.
fn read_bundle(exe: &Path) -> Option<String> {
    let mut file = File::open(exe).ok()?;
    let length = file.metadata().ok()?.len();
    let footer_start = length.checked_sub(16)?;
    file.seek(SeekFrom::Start(footer_start)).ok()?;
    let mut footer = [0u8; 16];
    file.read_exact(&mut footer).ok()?;
    if &footer[8..] != MAGIC {
        return None;
    }
    let size = u64::from_le_bytes(footer[..8].try_into().ok()?);
    file.seek(SeekFrom::Start(footer_start.checked_sub(size)?))
        .ok()?;
    let mut bundle = vec![0; usize::try_from(size).ok()?];
    file.read_exact(&mut bundle).ok()?;
    String::from_utf8(bundle).ok()
}

/// Builds the program at `path` into an executable at `output`, or by default in the current
/// directory, named after the package or the source file.
pub(crate) fn build(path: &Path, output: Option<&Path>) -> ExitCode {
    let Some(analysis) = crate::analyze(path, true) else {
        return ExitCode::FAILURE;
    };
    match pika_driver::runnable(&analysis) {
        Ok(_) => {}
        Err(pika_driver::RunError::InvalidProgram) => return ExitCode::FAILURE,
        Err(pika_driver::RunError::Unsupported(diagnostics)) => {
            crate::report(&diagnostics, &analysis.sources);
            return ExitCode::FAILURE;
        }
    }
    if !analysis.sources.root_package().binary {
        eprintln!(
            "error: {} is a library, not a program: a program has src/main.pk",
            path.display()
        );
        return ExitCode::FAILURE;
    }
    let output = output.map_or_else(
        || default_output(path, &analysis.sources),
        Path::to_path_buf,
    );
    match write_executable(&output, &analysis.sources.to_bundle()) {
        Ok(()) => {
            eprintln!("built {}", output.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: cannot write {}: {error}", output.display());
            ExitCode::FAILURE
        }
    }
}

/// Where an executable goes by default: in the current directory, named after the package,
/// or after the file of a program written in one file.
fn default_output(path: &Path, sources: &Sources) -> PathBuf {
    let name = if path.is_dir() {
        sources.root_package().name.clone()
    } else {
        path.file_stem().map_or_else(
            || "program".to_owned(),
            |stem| stem.to_string_lossy().into_owned(),
        )
    };
    PathBuf::from(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

/// Writes a copy of this executable carrying `bundle` to `output`, which can be run.
fn write_executable(output: &Path, bundle: &str) -> std::io::Result<()> {
    let mut bytes = std::fs::read(std::env::current_exe()?)?;
    bytes.extend_from_slice(bundle.as_bytes());
    bytes.extend_from_slice(&(bundle.len() as u64).to_le_bytes());
    bytes.extend_from_slice(MAGIC);
    std::fs::write(output, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(output, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}
