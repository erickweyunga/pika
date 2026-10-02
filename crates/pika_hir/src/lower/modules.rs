//! Modules and packages (spec section 13): the module tree, the names in scope in each
//! module, `:use` imports, paths and visibility.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use pika_diagnostics::{Diagnostic, Span};
use pika_syntax::ast::{self, AstNode};

use super::{TraitNames, TypeNames, ValueItem};
use crate::{FnId, Module, ModuleDef, ModuleId, TraitId, Ty, TypeHead, TypeId, codes, is_private};

/// A package to lower.
#[derive(Clone, Debug)]
pub struct SourcePackage {
    /// The package's name: the first segment of the paths of its items.
    pub name: String,
    /// The names of the packages it depends on, whose items its paths can also name.
    pub dependencies: Vec<String>,
    /// Its modules, one per source file.
    pub modules: Vec<SourceModule>,
}

/// The package being compiled, among the packages of a program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Root {
    /// Its index.
    pub package: usize,
    /// Whether it is a binary, whose root module is the program's entry module.
    pub binary: bool,
}

/// A source file to lower as a module of a package.
#[derive(Clone, Debug)]
pub struct SourceModule {
    /// The module's path in its package: empty for the package's root module (`src/main.pk`
    /// or `src/lib.pk`), and `["net", "http"]` for `src/net/http.pk`.
    pub path: Vec<String>,
    /// The parsed file.
    pub file: ast::SourceFile,
    /// Where the file starts in the program's source map: its spans are found with
    /// [`ast::with_span_base`].
    pub base: u32,
}

/// The items of one namespace each, by name.
#[derive(Clone, Default)]
pub(super) struct Items {
    /// Functions, called as `:name`.
    pub(super) fns: HashMap<String, FnId>,
    /// Types, with the type each names.
    pub(super) types: HashMap<String, (TypeId, Ty)>,
    /// Traits.
    pub(super) traits: HashMap<String, TraitId>,
    /// Constants and globals, read as `$name`.
    pub(super) values: HashMap<String, ValueItem>,
}

impl Items {
    /// Returns true if any namespace has an item called `name`.
    fn contains(&self, name: &str) -> bool {
        self.fns.contains_key(name)
            || self.types.contains_key(name)
            || self.traits.contains_key(name)
            || self.values.contains_key(name)
    }
}

/// What every module can see of the others: the items each declares, the module tree, and
/// the members of every type and trait.
#[derive(Default)]
pub(super) struct Shared {
    /// The items each module declares, which paths name.
    pub(super) declared: HashMap<ModuleId, Items>,
    /// The submodules of each module, by name.
    pub(super) children: HashMap<ModuleId, HashMap<String, ModuleId>>,
    /// The members of each type.
    pub(super) members: HashMap<TypeId, TypeNames>,
    /// The functions and required traits of each trait.
    pub(super) trait_members: HashMap<TraitId, TraitNames>,
    /// The user-defined traits listed in the `impl=` of each type.
    pub(super) type_impls: HashMap<TypeId, Vec<TraitId>>,
    /// The type of each declared type, with its own parameters as arguments.
    pub(super) type_tys: HashMap<TypeId, Ty>,
    /// The functions that `:impl` gives built-in types, by the head of the type and name.
    pub(super) impl_fns: HashMap<(TypeHead, String), FnId>,
}

impl Shared {
    /// The traits `ids` and the traits they require, transitively.
    pub(super) fn trait_closure(&self, ids: impl IntoIterator<Item = TraitId>) -> Vec<TraitId> {
        let mut found: Vec<TraitId> = Vec::new();
        let mut pending: Vec<TraitId> = ids.into_iter().collect();
        while let Some(id) = pending.pop() {
            if found.contains(&id) {
                continue;
            }
            found.push(id);
            if let Some(members) = self.trait_members.get(&id) {
                pending.extend(members.supertraits.iter().copied());
            }
        }
        found
    }

    /// The functions named `name` of the traits `traits`.
    pub(super) fn trait_functions(&self, traits: &[TraitId], name: &str) -> Vec<FnId> {
        traits
            .iter()
            .filter_map(|id| self.trait_members.get(id)?.functions.get(name).copied())
            .collect()
    }

    /// The type of the declared type `id`, with its own parameters as arguments.
    pub(super) fn type_ty(&self, id: TypeId) -> Ty {
        self.type_tys.get(&id).copied().unwrap_or(Ty::Error)
    }

    /// The module that the first segments of a path name: the first is a `:use` alias or a
    /// package, and each next one a submodule.
    fn resolve_module(
        &self,
        roots: &PathRoots<'_>,
        segments: &[(String, Span)],
    ) -> Result<ModuleId, Box<Diagnostic>> {
        let Some(((first, first_span), rest)) = segments.split_first() else {
            return Err(Box::new(Diagnostic::error(
                codes::UNRESOLVED_PATH,
                "expected a path",
                Span::empty(0),
            )));
        };
        // `:use` never declares an alias that is the name of a package.
        let mut module = match (roots.aliases.get(first), roots.packages.get(first)) {
            (Some(&module), _) | (None, Some(&module)) => module,
            (None, None) if roots.in_use => {
                return Err(Box::new(Diagnostic::error(
                    codes::UNRESOLVED_PATH,
                    format!("unknown package `{first}`"),
                    *first_span,
                )
                .with_help(
                    "the path of a `:use` starts with the name of this package or of a package it depends on",
                )));
            }
            (None, None) => {
                return Err(Box::new(Diagnostic::error(
                    codes::UNRESOLVED_PATH,
                    format!("unknown module or package `{first}`"),
                    *first_span,
                )
                .with_help(format!(
                    "paths start with a package name or a module imported with `:use`, as in `:use /std/{first}`"
                ))));
            }
        };
        let mut written = format!("/{first}");
        for (segment, span) in rest {
            let child = self
                .children
                .get(&module)
                .and_then(|children| children.get(segment));
            let Some(&child) = child else {
                return Err(Box::new(Diagnostic::error(
                    codes::UNRESOLVED_PATH,
                    format!("the module `{written}` has no module named `{segment}`"),
                    *span,
                )));
            };
            module = child;
            write_segment(&mut written, segment);
        }
        Ok(module)
    }

    /// The item a path names, found with `pick` in the items of its module; `what` describes
    /// the item for errors, as in "type".
    pub(super) fn resolve_item<T>(
        &self,
        roots: &PathRoots<'_>,
        segments: &[(String, Span)],
        what: &str,
        pick: impl Fn(&Items, &str) -> Option<T>,
    ) -> Result<T, Box<Diagnostic>> {
        let Some(((name, span), prefix)) = segments.split_last() else {
            return Err(Box::new(Diagnostic::error(
                codes::UNRESOLVED_PATH,
                "expected a path",
                Span::empty(0),
            )));
        };
        if prefix.is_empty() {
            return Err(Box::new(
                Diagnostic::error(
                    codes::UNRESOLVED_PATH,
                    format!("`/{name}` names a module, not a {what}"),
                    *span,
                )
                .with_help(format!("name an item of the module, as in `/{name}/item`")),
            ));
        }
        let module = self.resolve_module(roots, prefix)?;
        let items = self.declared.get(&module);
        let Some(found) = items.and_then(|items| pick(items, name)) else {
            let written = written_path(prefix);
            let mut diagnostic = Diagnostic::error(
                codes::UNRESOLVED_PATH,
                format!("the module `{written}` has no {what} named `{name}`"),
                *span,
            );
            if self
                .children
                .get(&module)
                .is_some_and(|children| children.contains_key(name))
            {
                diagnostic = diagnostic.with_help(format!(
                    "`{written}/{name}` is a module; import it with `:use {written}/{name}` to name its items"
                ));
            }
            return Err(Box::new(diagnostic));
        };
        check_visible(roots.module, module, name, *span, &written_path(prefix))?;
        Ok(found)
    }
}

/// Appends `/segment` to a written path.
fn write_segment(written: &mut String, segment: &str) {
    written.push('/');
    written.push_str(segment);
}

/// The path written as `segments`, as in `/std/math`.
fn written_path(segments: &[(String, Span)]) -> String {
    let mut written = String::new();
    for (segment, _) in segments {
        write_segment(&mut written, segment);
    }
    written
}

/// Reports the use of the private item `name` of `owner`, written `path`, from module `from`.
fn check_visible(
    from: ModuleId,
    owner: ModuleId,
    name: &str,
    span: Span,
    path: &str,
) -> Result<(), Box<Diagnostic>> {
    if from == owner || !is_private(name) {
        return Ok(());
    }
    Err(Box::new(
        Diagnostic::error(
            codes::PRIVATE_ITEM,
            format!("`{name}` is private to the module `{path}`"),
            span,
        )
        .with_help("names that start with `_` can only be used in the module that declares them"),
    ))
}

/// What the first segment of a path can name, in one module.
pub(super) struct PathRoots<'a> {
    /// The module the path is written in.
    module: ModuleId,
    /// The modules imported with `:use`, by alias.
    aliases: &'a HashMap<String, ModuleId>,
    /// The root modules of the packages the module can name.
    packages: &'a HashMap<String, ModuleId>,
    /// Whether the path is the path of a `:use`, which starts with a package name.
    in_use: bool,
}

/// The names in scope in one module: its own items, the prelude's and the ones it imports,
/// with the tables every module shares.
pub(super) struct ModuleNames {
    /// The module.
    pub(super) module: ModuleId,
    /// The items in scope, by name.
    pub(super) items: Items,
    /// The modules imported with `:use`, by alias.
    pub(super) aliases: HashMap<String, ModuleId>,
    /// The root modules of the packages the module can name: its own and its dependencies.
    pub(super) packages: HashMap<String, ModuleId>,
    /// Names of top-level `:local` variables of a script, for better error messages.
    pub(super) script_locals: HashSet<String>,
    /// What every module can see of the others.
    pub(super) shared: Rc<Shared>,
}

impl ModuleNames {
    /// What the first segment of a path written in the module can name.
    pub(super) fn roots(&self) -> PathRoots<'_> {
        PathRoots {
            module: self.module,
            aliases: &self.aliases,
            packages: &self.packages,
            in_use: false,
        }
    }

    /// The type a path such as `/geo/Point` names.
    pub(super) fn path_type(&self, path: &ast::Path) -> Result<(TypeId, Ty), Box<Diagnostic>> {
        self.shared
            .resolve_item(&self.roots(), &path.segments(), "type", |items, name| {
                items.types.get(name).copied()
            })
    }

    /// The trait a path such as `/geo/Shape` names.
    pub(super) fn path_trait(&self, path: &ast::Path) -> Result<TraitId, Box<Diagnostic>> {
        self.shared
            .resolve_item(&self.roots(), &path.segments(), "trait", |items, name| {
                items.traits.get(name).copied()
            })
    }

    /// The function a path such as `/math/sqrt` names.
    pub(super) fn path_fn(&self, path: &ast::Path) -> Result<FnId, Box<Diagnostic>> {
        self.shared.resolve_item(
            &self.roots(),
            &path.segments(),
            "function",
            |items, name| items.fns.get(name).copied(),
        )
    }

    /// Returns true if a path names a type rather than a value, as `/geo/Color` in
    /// `/geo/Color->red`. Errors are left to the lookup that follows.
    pub(super) fn path_is_type(&self, path: &ast::Path) -> bool {
        self.path_type(path).is_ok()
    }

    /// The value a path such as `/math/pi` names: a constant or a global, or a function,
    /// which is a value inside functions.
    pub(super) fn path_value(&self, path: &ast::Path) -> Result<PathValue, Box<Diagnostic>> {
        self.shared
            .resolve_item(&self.roots(), &path.segments(), "value", |items, name| {
                items
                    .values
                    .get(name)
                    .map(|&value| PathValue::Item(value))
                    .or_else(|| items.fns.get(name).map(|&id| PathValue::Fn(id)))
            })
    }
}

/// What a path in value position names.
#[derive(Clone, Copy)]
pub(super) enum PathValue {
    /// A constant or a global.
    Item(ValueItem),
    /// A function.
    Fn(FnId),
}

/// A module with a source file, to lower.
pub(super) struct Source<'p> {
    /// The module.
    pub(super) id: ModuleId,
    /// Its file.
    pub(super) file: &'p ast::SourceFile,
    /// Where the file starts in the program's source map.
    pub(super) base: u32,
    /// Whether it is the entry module of the program: the root module of the entry package.
    pub(super) entry: bool,
    /// The root modules of the packages it can name.
    pub(super) packages: HashMap<String, ModuleId>,
}

/// Adds the module tree of `packages` to `module`, with `root` as the package compiled, and
/// returns the modules to lower.
pub(super) fn add_modules<'p>(
    packages: &'p [SourcePackage],
    root: Root,
    module: &mut Module,
    shared: &mut Shared,
) -> Vec<Source<'p>> {
    let roots: HashMap<&str, ModuleId> = packages
        .iter()
        .map(|package| {
            let root = module.modules.alloc(ModuleDef {
                path: vec![package.name.clone()],
            });
            (package.name.as_str(), root)
        })
        .collect();
    module.root = packages
        .get(root.package)
        .map(|package| roots[package.name.as_str()]);
    debug_assert_eq!(roots.len(), packages.len(), "package names are distinct");
    let mut sources = Vec::new();
    for (index, package) in packages.iter().enumerate() {
        let visible: HashMap<String, ModuleId> = std::iter::once(&package.name)
            .chain(&package.dependencies)
            .filter_map(|name| Some((name.clone(), *roots.get(name.as_str())?)))
            .collect();
        for source in &package.modules {
            let mut id = roots[package.name.as_str()];
            for segment in &source.path {
                id = child_module(module, shared, id, segment);
            }
            sources.push(Source {
                id,
                file: &source.file,
                base: source.base,
                entry: root.binary && root.package == index && source.path.is_empty(),
                packages: visible.clone(),
            });
        }
    }
    sources
}

/// The submodule `name` of `parent`, added if it is new.
fn child_module(
    module: &mut Module,
    shared: &mut Shared,
    parent: ModuleId,
    name: &str,
) -> ModuleId {
    if let Some(&child) = shared.children.get(&parent).and_then(|c| c.get(name)) {
        return child;
    }
    let mut path = module.modules[parent].path.clone();
    path.push(name.to_owned());
    let child = module.modules.alloc(ModuleDef { path });
    shared
        .children
        .entry(parent)
        .or_default()
        .insert(name.to_owned(), child);
    child
}

/// The span of the declaration of the item `name` in the namespaces of `items`, for errors.
fn item_span(module: &Module, items: &Items, name: &str) -> Option<Span> {
    if let Some(&id) = items.fns.get(name) {
        return Some(module.functions[id].name.span);
    }
    if let Some(&(id, _)) = items.types.get(name) {
        return Some(module.types[id].name.span);
    }
    if let Some(&id) = items.traits.get(name) {
        return Some(module.traits[id].name.span);
    }
    items.values.get(name).map(|value| match *value {
        ValueItem::Const(id) => module.consts[id].name.span,
        ValueItem::Global(id) => module.globals[id].name.span,
    })
}

/// Adds the modules and items a `:use` imports to `names`. The first segment of its path
/// names a package: other imports do not affect it, so they can be written in any order.
pub(super) fn add_use(
    names: &mut ModuleNames,
    decl: &ast::UseDecl,
    shared: &Shared,
    module: &Module,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(path) = decl.path() else {
        // Reported by the parser.
        return;
    };
    let segments = path.segments();
    let Some(((last, last_span), prefix)) = segments.split_last() else {
        return;
    };
    let no_aliases = HashMap::new();
    let roots = PathRoots {
        module: names.module,
        aliases: &no_aliases,
        packages: &names.packages,
        in_use: true,
    };
    // `/package` alone imports the package's root module.
    let (target_module, items) = if prefix.is_empty() {
        match shared.resolve_module(&roots, &segments) {
            Ok(root) => (Some(root), None),
            Err(diagnostic) => {
                diagnostics.push(*diagnostic);
                return;
            }
        }
    } else {
        let parent = match shared.resolve_module(&roots, prefix) {
            Ok(parent) => parent,
            Err(diagnostic) => {
                diagnostics.push(*diagnostic);
                return;
            }
        };
        let child = shared
            .children
            .get(&parent)
            .and_then(|children| children.get(last))
            .copied();
        let items = shared
            .declared
            .get(&parent)
            .filter(|items| items.contains(last))
            .map(|items| (parent, items));
        (child, items)
    };
    let written = path.syntax().text().to_string();
    let alias = decl.alias().map_or_else(
        || (last.clone(), *last_span),
        |name| (name.text(), name.span()),
    );
    match (target_module, items) {
        (Some(_), Some(_)) => diagnostics.push(Diagnostic::error(
            codes::AMBIGUOUS_PATH,
            format!("`{written}` names both a module and an item"),
            path.span(),
        )),
        (Some(target), None) => import_module(names, target, alias, diagnostics),
        (None, Some((owner, items))) => {
            if let Err(diagnostic) =
                check_visible(names.module, owner, last, *last_span, &written_path(prefix))
            {
                diagnostics.push(*diagnostic);
                return;
            }
            import_items(names, items, last, alias, module, diagnostics);
        }
        (None, None) => diagnostics.push(Diagnostic::error(
            codes::UNRESOLVED_PATH,
            format!(
                "the module `{}` has no item or module named `{last}`",
                written_path(prefix)
            ),
            *last_span,
        )),
    }
}

/// Adds the module `target`, imported by a `:use`, to `names` under the alias `alias`.
fn import_module(
    names: &mut ModuleNames,
    target: ModuleId,
    alias: (String, Span),
    diagnostics: &mut Vec<Diagnostic>,
) {
    // `:use /package` names the package as it already is.
    if names.packages.get(&alias.0) == Some(&target) {
        return;
    }
    if names.packages.contains_key(&alias.0) {
        diagnostics.push(
            Diagnostic::error(
                codes::AMBIGUOUS_PATH,
                format!(
                    "`{}` is the name of a package, so it cannot name an imported module",
                    alias.0
                ),
                alias.1,
            )
            .with_help("give the module another name with `as=`"),
        );
        return;
    }
    if let Some(&previous) = names.aliases.get(&alias.0) {
        let what = if previous == target {
            "is imported more than once"
        } else {
            "names two imported modules"
        };
        diagnostics.push(Diagnostic::error(
            codes::DUPLICATE_ITEM,
            format!("`{}` {what}", alias.0),
            alias.1,
        ));
        return;
    }
    names.aliases.insert(alias.0, target);
}

/// Adds the items called `name` of `items` to `names`, under the name `alias`.
fn import_items(
    names: &mut ModuleNames,
    items: &Items,
    name: &str,
    (alias, span): (String, Span),
    module: &Module,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let taken = (items.fns.contains_key(name) && names.items.fns.contains_key(&alias))
        || ((items.types.contains_key(name) || items.traits.contains_key(name))
            && (names.items.types.contains_key(&alias) || names.items.traits.contains_key(&alias)))
        || (items.values.contains_key(name) && names.items.values.contains_key(&alias));
    if taken {
        let mut diagnostic = Diagnostic::error(
            codes::DUPLICATE_ITEM,
            format!("`{alias}` is already declared or imported in this module"),
            span,
        )
        .with_help(format!(
            "rename the import with `as=`, as in `as=other_{alias}`"
        ));
        if let Some(previous) = item_span(module, &names.items, &alias) {
            diagnostic = diagnostic.with_secondary(previous, "the other item");
        }
        diagnostics.push(diagnostic);
        return;
    }
    if let Some(&id) = items.fns.get(name) {
        names.items.fns.insert(alias.clone(), id);
    }
    if let Some(&ty) = items.types.get(name) {
        names.items.types.insert(alias.clone(), ty);
    }
    if let Some(&id) = items.traits.get(name) {
        names.items.traits.insert(alias.clone(), id);
    }
    if let Some(&value) = items.values.get(name) {
        names.items.values.insert(alias, value);
    }
}

/// The user-defined trait an `impl=` entry names, if any; errors are reported when the list
/// is lowered.
pub(super) fn listed_trait(
    names: &ModuleNames,
    shared: &Shared,
    item: &ast::Type,
) -> Option<TraitId> {
    let ast::Type::Path(path) = item else {
        return None;
    };
    if let Some(name) = path.name() {
        return names.items.traits.get(&name.text()).copied();
    }
    shared
        .resolve_item(
            &names.roots(),
            &path.path()?.segments(),
            "trait",
            |items, name| items.traits.get(name).copied(),
        )
        .ok()
}
