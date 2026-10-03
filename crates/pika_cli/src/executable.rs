//! `pika build`: compiles a program ahead of time into an executable that needs nothing
//! installed. The program is compiled to an object file, which the system linker links with
//! the runtime library that this executable carries.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use pika_driver::Sources;

/// The runtime library, `pika_runtime` as a static library, built by `build.rs`.
const RUNTIME_LIBRARY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/pika_runtime.lib.bin"));

/// The system libraries the runtime library needs, as linker arguments.
const SYSTEM_LIBRARIES: &str = include_str!(concat!(env!("OUT_DIR"), "/native-static-libs.txt"));

/// Builds the program at `path` into an executable at `output`, or by default in the current
/// directory, named after the package or the source file.
pub(crate) fn build(path: &Path, output: Option<&Path>) -> ExitCode {
    let Some(analysis) = crate::analyze(path, true) else {
        return ExitCode::FAILURE;
    };
    let program = match pika_driver::runnable(&analysis) {
        Ok(program) => program,
        Err(pika_driver::RunError::InvalidProgram) => return ExitCode::FAILURE,
        Err(pika_driver::RunError::Unsupported(diagnostics)) => {
            crate::report(&diagnostics, &analysis.sources);
            return ExitCode::FAILURE;
        }
    };
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
    let object = match pika_driver::compile_object(program, &analysis.sources.map) {
        Ok(object) => object,
        Err(error) => {
            eprintln!("internal compiler error: {error}");
            return ExitCode::from(70);
        }
    };
    match link(&object, &output) {
        Ok(()) => {
            eprintln!("built {}", output.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: cannot build {}: {error}", output.display());
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

/// A directory of temporary files, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> std::io::Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |time| time.subsec_nanos());
        let path = std::env::temp_dir().join(format!("pika-build-{}-{nanos}", std::process::id()));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Links the compiled program `object` with the runtime library into an executable at
/// `output`, with the system linker.
fn link(object: &[u8], output: &Path) -> Result<(), String> {
    let temp = TempDir::new().map_err(|e| format!("cannot create a temporary directory: {e}"))?;
    let object_path = temp.0.join(if cfg!(windows) {
        "program.obj"
    } else {
        "program.o"
    });
    let runtime_path = temp.0.join(if cfg!(target_env = "msvc") {
        "pika_runtime.lib"
    } else {
        "libpika_runtime.a"
    });
    for (path, bytes) in [(&object_path, object), (&runtime_path, RUNTIME_LIBRARY)] {
        std::fs::write(path, bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    }
    let mut command = linker_command(&object_path, &runtime_path, output)?;
    let result = command.output().map_err(|e| {
        format!(
            "cannot run the linker `{}`: {e}",
            command.get_program().display()
        )
    })?;
    if result.status.success() {
        Ok(())
    } else {
        Err(format!(
            "the linker `{}` failed:\n{}{}",
            command.get_program().display(),
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        ))
    }
}

/// The command that links `object` and `runtime` into `output`: the C compiler (`$CC`, or
/// `cc`), which runs the system linker, and with Visual C++, its linker.
#[cfg(not(target_env = "msvc"))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "finding the Visual C++ linker can fail, and both versions have one signature"
)]
fn linker_command(object: &Path, runtime: &Path, output: &Path) -> Result<Command, String> {
    let compiler = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let mut command = Command::new(compiler);
    command.arg("-o").arg(output).arg(object).arg(runtime);
    command.args(SYSTEM_LIBRARIES.split_whitespace());
    // Leave out the parts of the runtime the program does not use.
    if cfg!(target_vendor = "apple") {
        command.arg("-Wl,-dead_strip");
    } else {
        command.arg("-Wl,--gc-sections");
    }
    Ok(command)
}

#[cfg(target_env = "msvc")]
fn linker_command(object: &Path, runtime: &Path, output: &Path) -> Result<Command, String> {
    let linker = cc::windows_registry::find_tool(env!("PIKA_TARGET"), "link.exe").ok_or_else(|| {
        "cannot find the Visual C++ linker, `link.exe`: install the Visual Studio C++ build tools"
            .to_owned()
    })?;
    let mut command = linker.to_command();
    let mut out = std::ffi::OsString::from("/OUT:");
    out.push(output);
    command.args(["/NOLOGO", "/SUBSYSTEM:CONSOLE", "/OPT:REF"]);
    command.arg(out).arg(object).arg(runtime);
    command.args(SYSTEM_LIBRARIES.split_whitespace());
    Ok(command)
}
