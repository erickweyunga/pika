//! Runs the phases of the Pika compiler over the source files of a program.

mod package;

use std::fmt::Write;

use pika_diagnostics::{Diagnostic, SourceMap};
use pika_hir::{Bound, FnKind, Generics, Module, ModuleId, SourceModule, SourcePackage};
use pika_mir::InterpretError;
use pika_syntax::ast::{AstNode, SourceFile};
use pika_types::TypeckResult;

pub use package::{LoadError, MANIFEST, Package, Sources, load_package};

/// The result of analyzing a program.
#[derive(Debug)]
pub struct Analysis {
    /// The program's source files.
    pub sources: Sources,
    /// The lowered program, absent if a file has syntax errors.
    pub module: Option<Module>,
    /// Types, absent if a file has syntax errors.
    pub types: Option<TypeckResult>,
    /// The program in MIR form, present only if there are no errors.
    pub mir: Option<pika_mir::Program>,
    /// All diagnostics, ordered by position: by file, then within each file.
    pub diagnostics: Vec<Diagnostic>,
}

impl Analysis {
    /// Returns true if any diagnostic is an error.
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(Diagnostic::is_error)
    }
}

/// Parses, resolves and type checks a program written in one file, named `main.pk` in
/// reports.
pub fn check_source(text: &str) -> Analysis {
    check(Sources::single_file("main.pk", text))
}

/// Parses, resolves and type checks a program.
///
/// Name resolution and type checking only run on programs without syntax errors, so that one
/// typo does not cause a cascade of follow-up errors.
///
/// # Panics
///
/// Does not panic: the root of every parsed tree is a source file.
pub fn check(sources: Sources) -> Analysis {
    let mut diagnostics = Vec::new();
    let mut packages = Vec::new();
    for package in &sources.packages {
        let mut modules = Vec::new();
        for (path, file) in &package.modules {
            let file = sources.map.file(*file);
            let parse = pika_syntax::parse_at(&file.text, file.base);
            diagnostics.extend(parse.diagnostics().iter().cloned());
            modules.push(SourceModule {
                path: path.clone(),
                file: SourceFile::cast(parse.syntax()).expect("the root is a source file"),
                base: file.base,
            });
        }
        packages.push(SourcePackage {
            name: package.name.clone(),
            dependencies: package.dependencies.clone(),
            modules,
        });
    }
    if diagnostics.iter().any(Diagnostic::is_error) {
        diagnostics.sort_by_key(|d| d.primary.span.start);
        return Analysis {
            sources,
            module: None,
            types: None,
            mir: None,
            diagnostics,
        };
    }
    let root = pika_hir::Root {
        package: sources.root,
        binary: sources.root_package().binary,
    };
    let (module, lower_diagnostics) = pika_hir::lower_program(&packages, root);
    let types = pika_types::check_module(&module);
    diagnostics.extend(lower_diagnostics);
    diagnostics.extend(types.diagnostics.iter().cloned());

    // The MIR is only built for programs without errors so far.
    let mut mir = None;
    if !diagnostics.iter().any(Diagnostic::is_error) {
        let (program, mir_diagnostics) = pika_mir::build_program(&module, &types, &sources.map);
        diagnostics.extend(mir_diagnostics);
        if !diagnostics.iter().any(Diagnostic::is_error) {
            mir = Some(program);
        }
    }

    // Warnings are for the package being compiled, not its dependencies.
    diagnostics
        .retain(|d| d.is_error() || sources.is_root_file(sources.map.file_of(d.primary.span)));
    diagnostics.sort_by_key(|d| d.primary.span.start);
    Analysis {
        sources,
        module: Some(module),
        types: Some(types),
        mir,
        diagnostics,
    }
}

/// Why a program cannot be run.
#[derive(Debug)]
pub enum RunError {
    /// The program has errors; they are in the analysis' diagnostics.
    InvalidProgram,
    /// The program uses features the compiler cannot run yet.
    Unsupported(Vec<Diagnostic>),
}

/// Checks that an analyzed program can be run, and returns its MIR.
///
/// # Errors
///
/// Fails if the program has errors or uses features that cannot run yet.
pub fn runnable(analysis: &Analysis) -> Result<&pika_mir::Program, RunError> {
    let program = analysis.mir.as_ref().ok_or(RunError::InvalidProgram)?;
    if program.unsupported.is_empty() {
        return Ok(program);
    }
    // One diagnostic per feature, at its first use, so that a program using strings
    // everywhere gets a short report.
    let mut features: Vec<(&pika_mir::Unsupported, usize)> = Vec::new();
    for unsupported in &program.unsupported {
        match features
            .iter_mut()
            .find(|(first, _)| first.feature == unsupported.feature)
        {
            Some((first, count)) => {
                *count += 1;
                if unsupported.span.start < first.span.start {
                    *first = unsupported;
                }
            }
            None => features.push((unsupported, 1)),
        }
    }
    let mut diagnostics: Vec<Diagnostic> = features
        .into_iter()
        .map(|(first, count)| {
            let others = match count {
                1 => String::new(),
                2 => " (used once more in this file)".to_owned(),
                n => format!(" (used {} more times in this file)", n - 1),
            };
            Diagnostic::error(
                pika_mir::codes::NOT_RUNNABLE_YET,
                format!("{} cannot be run yet{others}", first.feature),
                first.span,
            )
            .with_help(format!(
                "planned for milestone {}; `pika check` still checks this program",
                first.milestone
            ))
        })
        .collect();
    diagnostics.sort_by_key(|d| d.primary.span.start);
    Err(RunError::Unsupported(diagnostics))
}

/// Runs a program with the MIR interpreter, with the arguments `args`, writing its output to
/// `out` and `err`. `sources` are its source files, which panic reports refer to.
///
/// Returns the process exit status: 0 on success, or 101 after a panic, whose report is
/// written to `err` exactly as a compiled program would write it.
pub fn interpret(
    program: &pika_mir::Program,
    sources: &SourceMap,
    args: Vec<String>,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> i32 {
    start_program(args);
    match pika_mir::interpret(program, out, err) {
        Ok(()) => 0,
        Err(InterpretError::Panic { message, span, .. }) => {
            let location = sources.locate(span);
            let report = pika_runtime::format::panic_report(
                &message,
                &sources.file(location.file).name,
                location.line,
                location.column,
            );
            // As in compiled programs, output written before the panic comes first.
            let _ = out.flush();
            let _ = err.write_all(report.as_bytes());
            pika_runtime::PANIC_EXIT_CODE
        }
        Err(InterpretError::Uncaught { errors }) => {
            let _ = out.flush();
            let report = pika_runtime::format::error_report(&errors);
            let _ = err.write_all(report.as_bytes());
            pika_runtime::ERROR_EXIT_CODE
        }
    }
}

/// Prepares the runtime for a program that starts now, with the arguments `args`.
fn start_program(args: Vec<String>) {
    pika_runtime::intrinsics::set_program_args(args);
    pika_runtime::intrinsics::start_clock();
}

/// Stack size of the thread that runs compiled programs, generous for deep recursion.
const PROGRAM_STACK_SIZE: usize = 256 * 1024 * 1024;

/// Compiles a program to native code and runs it in this process, with the arguments `args`.
/// `sources` are its source files, which panic reports refer to.
///
/// Returns 0 when the program finishes. A panic in the program prints its report and exits the
/// process with status 101. An internal error of the code generator returns 70. When the
/// environment variable `PIKA_LEAK_CHECK` is set, a program that finishes without freeing all
/// of its heap memory returns 102.
pub fn run_native(program: &pika_mir::Program, sources: &SourceMap, args: Vec<String>) -> i32 {
    start_program(args);
    pika_runtime::set_program_files(sources.files().map(|(_, file)| file.name.clone()).collect());
    std::thread::scope(|scope| {
        let runner = std::thread::Builder::new()
            .name("pika-main".to_owned())
            .stack_size(PROGRAM_STACK_SIZE)
            .spawn_scoped(scope, || match pika_codegen::compile(program, sources) {
                Ok(compiled) => {
                    pika_runtime::abi::set_stack_limit(PROGRAM_STACK_SIZE);
                    compiled.run();
                    pika_runtime::abi::finish();
                    pika_runtime::abi::leak_check().unwrap_or(0)
                }
                Err(error) => {
                    eprintln!("internal compiler error: {error}");
                    70
                }
            });
        match runner {
            Ok(handle) => handle.join().unwrap_or(70),
            Err(error) => {
                eprintln!("error: cannot start the program: {error}");
                70
            }
        }
    })
}

/// Describes the inferred types of a module: every constant, global and function with the
/// types of its locals, in declaration order. Used by `pika types` and by tests.
pub fn describe_types(analysis: &Analysis) -> String {
    let (Some(module), Some(types)) = (&analysis.module, &analysis.types) else {
        return "(not available: the file has syntax errors)\n".to_owned();
    };
    // Only the modules of the package compiled are described, not those of its
    // dependencies or the prelude.
    let package = module
        .root
        .and_then(|root| module.modules[root].path.first());
    let mut sorted: Vec<_> = module
        .modules
        .iter()
        .filter(|(_, def)| def.path.first() == package)
        .collect();
    sorted.sort_by(|(_, a), (_, b)| a.path.cmp(&b.path));
    let modules: Vec<(ModuleId, String)> = sorted
        .into_iter()
        .filter_map(|(id, def)| {
            let text = describe_module(module, types, id);
            (!text.is_empty()).then(|| (id, format!("module {}\n{text}", def.display_path())))
        })
        .collect();
    match modules.as_slice() {
        // A program of one module is described without a heading.
        [(id, _)] => describe_module(module, types, *id),
        several => several
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Describes the items of one module.
fn describe_module(module: &Module, types: &TypeckResult, id: ModuleId) -> String {
    let mut out = String::new();
    for (item_id, item) in module.consts.iter().filter(|(_, c)| c.module == id) {
        let ty = types
            .consts
            .get(item_id)
            .map_or("?".to_owned(), ToString::to_string);
        writeln!(out, "const {}: {ty}", item.name.value).expect("writing to a String");
    }
    for (item_id, item) in module.globals.iter().filter(|(_, g)| g.module == id) {
        let ty = types
            .globals
            .get(item_id)
            .map_or("?".to_owned(), ToString::to_string);
        writeln!(out, "global {}: {ty}", item.name.value).expect("writing to a String");
    }
    for (fn_id, function) in module.functions.iter().filter(|(_, f)| f.module == id) {
        describe_function(&mut out, module, types, fn_id, function);
    }
    out
}

/// Describes a function: its signature, then the type of each of its local variables.
fn describe_function(
    out: &mut String,
    module: &Module,
    types: &TypeckResult,
    id: pika_hir::FnId,
    function: &pika_hir::Function,
) {
    let body_types = types.functions.get(id);
    let local_ty = |local| {
        body_types
            .and_then(|t| t.locals.get(local))
            .map_or("?".to_owned(), ToString::to_string)
    };
    let params: Vec<String> = function
        .params
        .iter()
        .map(|p| {
            // A required function of a trait has no body: its parameters have their
            // declared types.
            let ty = if function.has_body {
                local_ty(p.local)
            } else {
                p.ty.value.to_string()
            };
            format!("{}: {ty}", function.body.locals[p.local].name.value)
        })
        .collect();
    let kind = match function.kind {
        FnKind::Declared => "fn",
        FnKind::ImplicitMain => "script",
        FnKind::Closure(_) => "closure",
        FnKind::Runtime => "runtime fn",
    };
    writeln!(
        out,
        "{kind} {}{}({}) -> {}",
        function.name.value,
        generics(module, &function.generics),
        params.join(", "),
        function.ret.value
    )
    .expect("writing to a String");
    for (local, data) in function.body.locals.iter() {
        if function.params.iter().any(|p| p.local == local) {
            continue;
        }
        writeln!(out, "    {}: {}", data.name.value, local_ty(local)).expect("writing to a String");
    }
}

/// Type parameters as declared, as in `<K: Hash + Eq, V>`, or nothing.
fn generics(module: &Module, generics: &Generics) -> String {
    if generics.is_empty() {
        return String::new();
    }
    let params: Vec<String> = generics
        .params
        .iter()
        .map(|param| {
            let bounds: Vec<String> = param
                .bounds
                .iter()
                .map(|bound| match bound.value {
                    Bound::Builtin(builtin) => builtin.name().to_owned(),
                    Bound::Trait(id) => module.traits[id].name.value.clone(),
                })
                .collect();
            if bounds.is_empty() {
                param.name.value.clone()
            } else {
                format!("{}: {}", param.name.value, bounds.join(" + "))
            }
        })
        .collect();
    format!("<{}>", params.join(", "))
}
