//! Running a compiled program: what happens before and after its `main`. Programs that the
//! compiler runs directly and executables made by `pika build` start the same way.

#![allow(
    unsafe_code,
    reason = "executables call into the runtime with raw pointers"
)]

use crate::{ProgramFile, abi, intrinsics};

/// The size of the stack a program runs on: generous, for deep recursion.
pub const PROGRAM_STACK_SIZE: usize = 256 * 1024 * 1024;

/// The exit status when a program cannot run because of a bug of the compiler or runtime.
pub const INTERNAL_ERROR_EXIT_CODE: i32 = 70;

/// A function of a compiled program that takes and returns nothing: its entry point, or the
/// function that destroys its globals when it ends.
pub type ProgramFn = extern "C" fn();

/// Prepares the runtime for a program that starts now, with the arguments `args` (without
/// the name of the program) and the source files that its reports point into.
pub fn prepare(args: Vec<String>, files: Vec<ProgramFile>) {
    intrinsics::set_program_args(args);
    intrinsics::start_clock();
    crate::set_program_files(files);
}

/// Runs a compiled program, prepared with [`prepare`], on a thread of its own with a large
/// stack: `entry`, then `finish`, which destroys its globals. A program without an entry
/// point does nothing.
///
/// Returns the exit status: 0, or under `PIKA_LEAK_CHECK` the leak status of a program that
/// did not free its memory. A panic, or an error that `main` raises, ends the process.
pub fn run(entry: Option<ProgramFn>, finish: ProgramFn) -> i32 {
    std::thread::scope(|scope| {
        let runner = std::thread::Builder::new()
            .name("pika-main".to_owned())
            .stack_size(PROGRAM_STACK_SIZE)
            .spawn_scoped(scope, || {
                abi::set_stack_limit(PROGRAM_STACK_SIZE);
                if let Some(entry) = entry {
                    entry();
                    finish();
                }
                abi::finish();
                abi::leak_check().unwrap_or(0)
            });
        match runner {
            Ok(handle) => handle.join().unwrap_or(INTERNAL_ERROR_EXIT_CODE),
            Err(error) => {
                eprintln!("error: cannot start the program: {error}");
                INTERNAL_ERROR_EXIT_CODE
            }
        }
    })
}

/// The source files of a program, encoded for an executable: for each file, the length of
/// its name, its name, the length of its text and its text, lengths as little-endian `u32`.
///
/// # Panics
///
/// Panics if a name or text is 4 GiB or larger, which the compiler does not accept.
pub fn encode_files(files: &[ProgramFile]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for file in files {
        for part in [&file.name, &file.text] {
            let length = u32::try_from(part.len()).expect("source files are smaller than 4 GiB");
            bytes.extend_from_slice(&length.to_le_bytes());
            bytes.extend_from_slice(part.as_bytes());
        }
    }
    bytes
}

/// The source files encoded by [`encode_files`], or as many of them as are whole.
pub fn decode_files(mut bytes: &[u8]) -> Vec<ProgramFile> {
    let mut part = || -> Option<String> {
        let (length, rest) = bytes.split_first_chunk::<4>()?;
        let length = usize::try_from(u32::from_le_bytes(*length)).ok()?;
        let text = rest.get(..length)?;
        bytes = &rest[length..];
        Some(String::from_utf8_lossy(text).into_owned())
    };
    let mut files = Vec::new();
    while let (Some(name), Some(text)) = (part(), part()) {
        files.push(ProgramFile { name, text });
    }
    files
}

/// The `main` of an executable made by `pika build`, which its C `main` calls: runs the
/// program, whose entry point is `entry` and whose source files, encoded by
/// [`encode_files`], are the `files_len` bytes at `files`. Returns the exit status.
///
/// # Safety
///
/// `entry` and `finish` must be functions of the compiled program, and `files` must point to
/// `files_len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pika_main(
    entry: Option<ProgramFn>,
    finish: ProgramFn,
    files: *const u8,
    files_len: usize,
) -> i32 {
    ignore_broken_pipes();
    // SAFETY: guaranteed by the caller.
    let files = unsafe { std::slice::from_raw_parts(files, files_len) };
    prepare(std::env::args().skip(1).collect(), decode_files(files));
    run(entry, finish)
}

/// Makes writing to a closed pipe an error instead of a signal that ends the process, as
/// for programs the compiler runs: their output is lost, and they go on.
fn ignore_broken_pipes() {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn signal(signal: i32, handler: usize) -> usize;
        }
        const SIGPIPE: i32 = 13;
        const SIG_IGN: usize = 1;
        // SAFETY: ignoring a signal is always allowed; nothing else handles `SIGPIPE`.
        unsafe { signal(SIGPIPE, SIG_IGN) };
    }
}

#[cfg(test)]
mod tests {
    use super::{decode_files, encode_files};
    use crate::ProgramFile;

    #[test]
    fn files_survive_encoding() {
        let files = vec![
            ProgramFile {
                name: "src/main.pk".to_owned(),
                text: ":put \"héllo\"\n".to_owned(),
            },
            ProgramFile {
                name: String::new(),
                text: String::new(),
            },
        ];
        assert_eq!(decode_files(&encode_files(&files)), files);
        assert!(decode_files(&[1, 0]).is_empty());
    }
}
