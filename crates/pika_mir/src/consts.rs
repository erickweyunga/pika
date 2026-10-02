//! Compile-time evaluation of constants, global initializers and default parameter values.
//!
//! Each initializer is lowered to a small MIR body in constant-evaluation mode and run by the
//! interpreter.

use std::collections::HashMap;

use la_arena::ArenaMap;
use pika_diagnostics::{Diagnostic, Span};
use pika_hir::{self as hir, ConstId, FnId, GlobalId, TypeId};
use pika_types::{BodyTypes, Ty, TypeckResult};

use crate::build::{Builder, Mode};
use crate::interp::{self, InterpretError};
use crate::{Body, Statement, Types, Value, codes};

pub(crate) struct ConstValues<'a> {
    module: &'a hir::Module,
    types: &'a TypeckResult,
    structs: &'a Types,
    consts: ArenaMap<ConstId, Value>,
    in_progress: ArenaMap<ConstId, ()>,
    globals: ArenaMap<GlobalId, Value>,
    defaults: HashMap<(FnId, usize), Value>,
    field_defaults: HashMap<(TypeId, usize), Value>,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> ConstValues<'a> {
    pub(crate) fn new(
        module: &'a hir::Module,
        types: &'a TypeckResult,
        structs: &'a Types,
    ) -> Self {
        Self {
            module,
            types,
            structs,
            consts: ArenaMap::default(),
            in_progress: ArenaMap::default(),
            globals: ArenaMap::default(),
            defaults: HashMap::new(),
            field_defaults: HashMap::new(),
            diagnostics: Vec::new(),
        }
    }

    pub(crate) fn into_diagnostics(self) -> Vec<Diagnostic> {
        self.diagnostics
    }

    /// The value of a module-level constant.
    pub(crate) fn const_value(&mut self, id: ConstId) -> Value {
        if let Some(value) = self.consts.get(id) {
            return value.clone();
        }
        // Cycles are rejected by the type checker, so this only guards against bugs.
        if self.in_progress.get(id).is_some() {
            return Value::Nothing;
        }
        self.in_progress.insert(id, ());
        let module = self.module;
        let item = &module.consts[id];
        let value = match (item.init, self.types.const_bodies.get(id)) {
            (Some(init), Some(types)) => self.evaluate(
                &item.body,
                types,
                init,
                &item.name.value,
                item.name.span,
                "constant",
            ),
            _ => Value::Nothing,
        };
        self.consts.insert(id, value.clone());
        value
    }

    /// The initial value of a module-level variable.
    pub(crate) fn global_value(&mut self, id: GlobalId) -> Value {
        if let Some(value) = self.globals.get(id) {
            return value.clone();
        }
        let module = self.module;
        let item = &module.globals[id];
        let value = match (item.init, self.types.global_bodies.get(id)) {
            (Some(init), Some(types)) => self.evaluate(
                &item.body,
                types,
                init,
                &item.name.value,
                item.name.span,
                "global",
            ),
            _ => Value::Nothing,
        };
        self.globals.insert(id, value.clone());
        value
    }

    /// The default value of parameter `index` of a function.
    pub(crate) fn default_value(&mut self, function: FnId, index: usize) -> Value {
        if let Some(value) = self.defaults.get(&(function, index)) {
            return value.clone();
        }
        let module = self.module;
        let item = &module.functions[function];
        let value = match (
            item.params.get(index).and_then(|p| p.default),
            self.types.functions.get(function),
        ) {
            (Some(default), Some(types)) => {
                let param = &item.body.locals[item.params[index].local].name;
                self.evaluate(
                    &item.body,
                    types,
                    default,
                    &param.value,
                    param.span,
                    "default value of",
                )
            }
            _ => Value::Nothing,
        };
        self.defaults.insert((function, index), value.clone());
        value
    }

    /// The default value of field `index` of a struct.
    pub(crate) fn field_default(&mut self, strukt: TypeId, index: usize) -> Value {
        if let Some(value) = self.field_defaults.get(&(strukt, index)) {
            return value.clone();
        }
        let module = self.module;
        let item = &module.types[strukt];
        let value = match (
            item.fields.get(index).and_then(|f| f.default),
            self.types.type_bodies.get(strukt),
        ) {
            (Some(default), Some(types)) => {
                let field = &item.fields[index].name;
                self.evaluate(
                    &item.body,
                    types,
                    default,
                    &field.value,
                    field.span,
                    "default value of field",
                )
            }
            _ => Value::Nothing,
        };
        self.field_defaults.insert((strukt, index), value.clone());
        value
    }

    fn evaluate(
        &mut self,
        body: &'a hir::Body,
        types: &'a BodyTypes,
        expr: hir::ExprId,
        name: &str,
        name_span: Span,
        what: &str,
    ) -> Value {
        let module = self.module;
        let structs = self.structs;
        let builder = Builder::new(module, structs, body, types, self, Mode::ConstEval);
        let mut mir = builder.build_const(expr, name);
        // Drops of temporaries moved into the result are elaborated as in functions.
        let errors = crate::ownership::analyze(&mut mir);
        if !errors.is_empty() {
            self.diagnostics.extend(errors);
            return Value::Nothing;
        }
        // Destroying such a value would run a function, which constants cannot.
        if let Some(ty) = destroyed_with_drop(structs, &mir) {
            self.diagnostics.push(
                Diagnostic::error(
                    codes::DROP_IN_CONSTANT,
                    format!(
                        "evaluating the {what} `{name}` would destroy a value of type `{ty}`, which runs `drop`"
                    ),
                    name_span,
                )
                .with_help("compute the value at run time instead, in a function"),
            );
            return Value::Nothing;
        }
        match interp::eval_body(&mir, structs) {
            Ok(value) => value,
            Err(InterpretError::Uncaught { .. }) => {
                unreachable!("constants cannot raise errors")
            }
            Err(InterpretError::Panic { message, span, .. }) => {
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::CONST_EVALUATION_FAILED,
                        format!("evaluating the {what} `{name}` failed: {message}"),
                        span,
                    )
                    .with_secondary(name_span, "while evaluating this at compile time"),
                );
                Value::Nothing
            }
        }
    }
}

/// The type of a value with a `drop` function that `body` destroys, if there is one.
fn destroyed_with_drop(structs: &Types, body: &Body) -> Option<Ty> {
    body.blocks
        .iter()
        .flat_map(|(_, block)| &block.statements)
        .find_map(|statement| match statement {
            Statement::Drop { place, .. } => {
                let ty = crate::place_ty(structs, body, place, &|_| Ty::Error);
                structs.user_drops(ty).first().copied()
            }
            _ => None,
        })
}
