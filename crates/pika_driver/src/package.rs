//! The source files of programs: a single file, or a package with its dependencies (spec
//! section 13.1).

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};

use pika_diagnostics::{FileId, SourceMap};
use serde::Deserialize;

/// The name of a package's manifest file.
pub const MANIFEST: &str = "pika.toml";

/// The standard library's files, embedded in the compiler: each one's path in the `std`
/// directory and its text.
const STD_FILES: &[(&str, &str)] = include!(concat!(env!("OUT_DIR"), "/std_files.rs"));

/// The source files of a program, and the packages they form.
#[derive(Debug, Default)]
pub struct Sources {
    /// Every source file.
    pub map: SourceMap,
    /// The packages, each after the packages it depends on.
    pub packages: Vec<Package>,
    /// The index of the package being compiled; the others are its dependencies.
    pub root: usize,
}

/// A package: its name, its dependencies and its modules.
#[derive(Debug)]
pub struct Package {
    /// The name, which paths to its items start with.
    pub name: String,
    /// The version given in the manifest.
    pub version: Option<String>,
    /// Whether it is a program, with the entry module `src/main.pk`, rather than a library.
    pub binary: bool,
    /// The names of the packages it depends on.
    pub dependencies: Vec<String>,
    /// Each module: its path in the package, empty for the root module, and its file.
    pub modules: Vec<(Vec<String>, FileId)>,
}

impl Sources {
    /// The package being compiled.
    pub fn root_package(&self) -> &Package {
        &self.packages[self.root]
    }

    /// The standard library alone, as the package compiled.
    pub fn standard_library() -> Self {
        let mut sources = Self::default();
        sources.root = sources.add_standard_library();
        sources
    }

    /// A program written in one file, `name`, with the text `text`: the root module of a
    /// binary package named [`pika_hir::SINGLE_FILE_PACKAGE`], which uses the standard library.
    pub fn single_file(name: impl Into<String>, text: impl Into<String>) -> Self {
        let mut sources = Self::default();
        sources.add_standard_library();
        let file = sources.map.add(name, text);
        sources.root = sources.packages.len();
        sources.packages.push(Package {
            name: pika_hir::SINGLE_FILE_PACKAGE.to_owned(),
            version: None,
            binary: true,
            dependencies: vec![pika_hir::STD_PACKAGE.to_owned()],
            modules: vec![(Vec::new(), file)],
        });
        sources
    }

    /// Adds the standard library's package; returns its index.
    fn add_standard_library(&mut self) -> usize {
        let modules = STD_FILES
            .iter()
            .map(|&(path, text)| {
                let module: Vec<String> = match path.strip_suffix(".pk") {
                    Some("lib") | None => Vec::new(),
                    Some(module) => module.split('/').map(str::to_owned).collect(),
                };
                let file = self
                    .map
                    .add(format!("{}/{path}", pika_hir::STD_PACKAGE), text);
                (module, file)
            })
            .collect();
        self.packages.push(Package {
            name: pika_hir::STD_PACKAGE.to_owned(),
            version: None,
            binary: false,
            dependencies: Vec::new(),
            modules,
        });
        self.packages.len() - 1
    }

    /// Returns true if `file` is in the package being compiled, rather than in a dependency.
    pub fn is_root_file(&self, file: FileId) -> bool {
        self.root_package()
            .modules
            .iter()
            .any(|&(_, root_file)| root_file == file)
    }
}

/// Why a package cannot be loaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadError(pub String);

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LoadError {}

/// A package's manifest, `pika.toml`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    package: PackageSection,
    #[serde(default)]
    dependencies: BTreeMap<String, Dependency>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PackageSection {
    name: String,
    version: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Dependency {
    /// The package's directory, relative to the manifest's.
    path: PathBuf,
}

/// Loads the package in the directory `dir` and the packages it depends on.
///
/// # Errors
///
/// Fails if a manifest is missing or invalid, a source file cannot be read or has a name
/// that is not an identifier, or the dependencies form a cycle.
pub fn load_package(dir: &Path) -> Result<Sources, LoadError> {
    let mut loader = Loader::default();
    loader.sources.add_standard_library();
    let index = loader.load(dir, None)?;
    let mut sources = loader.sources;
    sources.root = index;
    Ok(sources)
}

#[derive(Default)]
struct Loader {
    sources: Sources,
    /// The packages loaded, by canonical directory.
    loaded: HashMap<PathBuf, usize>,
    /// The packages being loaded, outermost first, to find cycles.
    loading: Vec<PathBuf>,
    /// The directory of each package loaded from one, by index, for errors.
    dirs: HashMap<usize, PathBuf>,
}

impl Loader {
    /// Loads the package in `dir`, as the dependency `dependency` of another package if
    /// given; returns its index.
    fn load(&mut self, dir: &Path, dependency: Option<&str>) -> Result<usize, LoadError> {
        let canonical = dir.canonicalize().map_err(|error| {
            LoadError(format!(
                "cannot open the package directory {}: {error}",
                dir.display()
            ))
        })?;
        if let Some(&index) = self.loaded.get(&canonical) {
            return Ok(index);
        }
        if self.loading.contains(&canonical) {
            return Err(LoadError(format!(
                "the dependencies of {} form a cycle",
                dir.display()
            )));
        }
        let manifest = read_manifest(dir)?;
        let name = manifest.package.name;
        if name == pika_hir::STD_PACKAGE {
            return Err(LoadError(format!(
                "the package in {} is named `{name}`, which is the name of the standard library",
                dir.display()
            )));
        }
        if !pika_syntax::is_identifier(&name) {
            return Err(LoadError(format!(
                "the package name `{name}` in {} is not an identifier: use letters, digits and `_`, as in `my_package`",
                dir.join(MANIFEST).display()
            )));
        }
        if let Some(expected) = dependency
            && expected != name
        {
            return Err(LoadError(format!(
                "the dependency `{expected}` is the package `{name}` in {}: name the dependency after the package",
                dir.display()
            )));
        }
        let (root, binary) = root_file(dir, dependency.is_some())?;

        self.loading.push(canonical.clone());
        let mut dependencies = vec![pika_hir::STD_PACKAGE.to_owned()];
        for (dependency, spec) in &manifest.dependencies {
            self.load(&normalize(&dir.join(&spec.path)), Some(dependency))?;
            dependencies.push(dependency.clone());
        }
        self.loading.pop();

        if let Some(other) = self.sources.packages.iter().position(|p| p.name == name) {
            return Err(LoadError(format!(
                "two different packages are named `{name}`: one in {}, the other in {}",
                self.dirs[&other].display(),
                dir.display()
            )));
        }
        let modules = self.load_modules(dir, &root)?;
        let index = self.sources.packages.len();
        self.sources.packages.push(Package {
            name,
            version: manifest.package.version,
            binary,
            dependencies,
            modules,
        });
        self.loaded.insert(canonical, index);
        self.dirs.insert(index, dir.to_owned());
        Ok(index)
    }

    /// Reads the modules of the package in `dir`, whose root module is the file `root`.
    fn load_modules(
        &mut self,
        dir: &Path,
        root: &Path,
    ) -> Result<Vec<(Vec<String>, FileId)>, LoadError> {
        let src = dir.join("src");
        let mut files = Vec::new();
        find_sources(&src, &mut files)?;
        files.sort();
        let mut modules = Vec::new();
        for file in files {
            let module_path = if file == root {
                Vec::new()
            } else {
                module_path(&src, &file)?
            };
            let text = std::fs::read_to_string(&file)
                .map_err(|error| LoadError(format!("cannot read {}: {error}", file.display())))?;
            let id = self.sources.map.add(display_name(&file), text);
            modules.push((module_path, id));
        }
        Ok(modules)
    }
}

fn read_manifest(dir: &Path) -> Result<Manifest, LoadError> {
    let path = dir.join(MANIFEST);
    let text = std::fs::read_to_string(&path).map_err(|error| {
        LoadError(format!(
            "cannot read the package manifest {}: {error}",
            path.display()
        ))
    })?;
    toml::from_str(&text)
        .map_err(|error| LoadError(format!("invalid manifest {}: {error}", path.display())))
}

/// The root module's file of the package in `dir`: `src/main.pk` for a binary, `src/lib.pk`
/// for a library; and whether it is a binary. A dependency must be a library.
fn root_file(dir: &Path, is_dependency: bool) -> Result<(PathBuf, bool), LoadError> {
    let main = dir.join("src").join("main.pk");
    let lib = dir.join("src").join("lib.pk");
    match (main.is_file(), lib.is_file()) {
        (true, true) => Err(LoadError(format!(
            "the package in {} has both src/main.pk and src/lib.pk; a package is either a program or a library",
            dir.display()
        ))),
        (true, false) if is_dependency => Err(LoadError(format!(
            "the package in {} is a program (src/main.pk), not a library (src/lib.pk), so it cannot be a dependency",
            dir.display()
        ))),
        (true, false) => Ok((main, true)),
        (false, true) => Ok((lib, false)),
        (false, false) => Err(LoadError(format!(
            "the package in {} has neither src/main.pk nor src/lib.pk",
            dir.display()
        ))),
    }
}

/// Adds the `.pk` files under `dir` to `files`, recursively.
fn find_sources(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), LoadError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|error| LoadError(format!("cannot read {}: {error}", dir.display())))?;
    for entry in entries {
        let path = entry
            .map_err(|error| LoadError(format!("cannot read {}: {error}", dir.display())))?
            .path();
        let hidden = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with('.'));
        if hidden {
            continue;
        }
        if path.is_dir() {
            find_sources(&path, files)?;
        } else if path.extension().is_some_and(|extension| extension == "pk") {
            files.push(path);
        }
    }
    Ok(())
}

/// The path of the module in `file`, relative to the package's `src` directory: `net/http.pk`
/// is the module `["net", "http"]`.
fn module_path(src: &Path, file: &Path) -> Result<Vec<String>, LoadError> {
    let relative = file.strip_prefix(src).unwrap_or(file).with_extension("");
    let mut path = Vec::new();
    for component in relative.components() {
        let segment = component.as_os_str().to_string_lossy().into_owned();
        if !pika_syntax::is_identifier(&segment) {
            return Err(LoadError(format!(
                "the module file {} has a name that is not an identifier: `{segment}`; use letters, digits and `_`",
                file.display()
            )));
        }
        path.push(segment);
    }
    Ok(path)
}

/// `path` with each `..` that follows a directory name removed with that name, as in
/// `a/b/../c` to `a/c`. Symbolic links are not followed: the result is for messages and
/// file names, while packages are identified by their canonical directory.
fn normalize(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        let follows_name = matches!(
            normal.components().next_back(),
            Some(std::path::Component::Normal(_))
        );
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir if follows_name => {
                normal.pop();
            }
            other => normal.push(other),
        }
    }
    normal
}

/// The name of a source file in reports: its path, without a leading `./`, with `/` between
/// its parts on every platform, so that reports are the same everywhere.
fn display_name(file: &Path) -> String {
    let name = file.strip_prefix(".").unwrap_or(file).display().to_string();
    if std::path::MAIN_SEPARATOR == '/' {
        name
    } else {
        name.replace(std::path::MAIN_SEPARATOR, "/")
    }
}
