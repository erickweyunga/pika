//! An interpreter for MIR.
//!
//! It evaluates constants at compile time, and runs whole programs as a reference for the code
//! generator: a program must behave the same when interpreted and when compiled. Calls use an
//! explicit stack of frames, so deep recursion in the program cannot overflow the compiler's
//! own stack.
//!
//! The interpreter also checks the MIR's ownership bookkeeping: reading a moved or dropped
//! value, or dropping a value that is not there, is an internal error rather than undefined
//! behavior.

use std::collections::HashMap;
use std::sync::Arc;

use std::io::Write;

use la_arena::{Arena, ArenaMap};
use pika_diagnostics::Span;
use pika_hir::GlobalId;
use pika_runtime::intrinsics::{self, Arg, Io, Ret};
use pika_types::Ty;

use crate::value::{FunctionValue, binary, cast, unary};
use crate::{
    BlockId, Body, CallArg, CallTarget, ErrorTarget, GlobalInit, InstanceId, LocalId, LocalMode,
    Operand, PanicKind, Place, PrintPart, Program, Rvalue, Statement, Stream, Terminator, Types,
    Value,
};

/// Calls deeper than this stop the interpreted program with a stack overflow.
const MAX_CALL_DEPTH: usize = 100_000;

/// Why interpretation stopped early.
#[derive(Clone, Debug, PartialEq)]
pub enum InterpretError {
    /// The program panicked.
    Panic {
        /// Why.
        kind: PanicKind,
        /// The full message, as printed after `panic: `.
        message: String,
        /// Where in the source.
        span: Span,
    },
    /// `main` raised an error.
    Uncaught {
        /// The error, then each error that caused it.
        errors: Vec<pika_runtime::format::RaisedError>,
    },
}

/// The variable holding a storage location: a local of some frame, or a global.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Root {
    Local {
        frame: usize,
        local: LocalId,
    },
    Global(GlobalId),
    /// A value being destroyed outside any variable, such as a key discarded by a map,
    /// while its `drop` functions run.
    Scratch(usize),
    /// A value captured by the function value running in a frame.
    Capture {
        frame: usize,
        index: u32,
    },
}

/// A storage location: a variable, or a part of one.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Location {
    root: Root,
    /// The steps from the variable's value to the location.
    path: Vec<Step>,
}

/// One step from a value to a part of it, with positions known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Field(u32),
    VariantField(u32, u32),
    Deref,
    Element(usize),
    Key(usize),
    Value(usize),
}

/// The content of a local slot.
#[derive(Clone, Debug)]
enum Slot {
    Value(Value),
    /// A borrowed parameter: refers to a location in a caller's frame or a global.
    Ref(Location),
}

struct Frame<'p> {
    body: &'p Body,
    locals: ArenaMap<LocalId, Slot>,
    block: BlockId,
    /// Where the caller wants the result, and where it continues.
    return_to: Option<(Place, BlockId)>,
    /// Where the caller wants an error raised by this function, and where it continues.
    error_to: Option<(Place, BlockId)>,
    /// For the body of a function value: the function value, whose captures it reads.
    env: Option<Arc<FunctionValue>>,
}

/// The streams of the interpreter, for the intrinsics that read and write them.
struct MachineIo<'a> {
    out: &'a mut dyn Write,
    err: &'a mut dyn Write,
}

impl Io for MachineIo<'_> {
    fn write_out(&mut self, text: &str) {
        let _ = self.out.write_all(text.as_bytes());
    }

    fn write_err(&mut self, text: &str) {
        let _ = self.err.write_all(text.as_bytes());
    }

    fn read_line(&mut self) -> String {
        let _ = self.out.flush();
        intrinsics::read_stdin_line()
    }
}

/// The argument of an intrinsic for a value.
fn intrinsic_arg(value: &Value) -> Arg<'_> {
    match value {
        Value::Int { value, .. } => Arg::Int(i64::try_from(*value).expect("intrinsics take i64")),
        Value::Float { value, .. } => Arg::Float(*value),
        Value::Bool(value) => Arg::Bool(*value),
        Value::Char(value) => Arg::Char(*value),
        Value::Str(text) => Arg::Str(text),
        Value::Duration(nanos) => Arg::Duration(*nanos),
        other => unreachable!("intrinsics do not take {other:?}"),
    }
}

/// The value of the result of an intrinsic.
fn intrinsic_value(ret: Ret) -> Value {
    match ret {
        Ret::Int(value) => Value::Int {
            value: i128::from(value),
            ty: pika_hir::IntTy::I64,
        },
        Ret::Float(value) => Value::Float {
            value,
            ty: pika_hir::FloatTy::F64,
        },
        Ret::Bool(value) => Value::Bool(value),
        Ret::Char(value) => Value::Char(value),
        Ret::Str(text) => Value::Str(text.into()),
        Ret::Duration(nanos) => Value::Duration(nanos),
        Ret::Nothing => Value::Nothing,
    }
}

struct Machine<'p, 'w> {
    functions: Option<&'p Arena<Body>>,
    types: &'p Types,
    /// The `drop` function of each type that has one.
    drop_fns: Option<&'p HashMap<Ty, InstanceId>>,
    /// The `fmt` function of each type that has one.
    display_fns: Option<&'p HashMap<Ty, InstanceId>>,
    /// Whether values of each type met so far contain values with `drop` functions.
    has_drops: HashMap<Ty, bool>,
    /// Values being destroyed outside any variable.
    scratch: Vec<Option<Value>>,
    globals: ArenaMap<GlobalId, Value>,
    stack: Vec<Frame<'p>>,
    out: &'w mut dyn Write,
    err: &'w mut dyn Write,
}

/// Runs a program from its entry point, writing its output to `out` and `err`.
///
/// # Errors
///
/// Returns the panic that stopped the program.
pub fn interpret(
    program: &Program,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), InterpretError> {
    let Some(entry) = program.entry.map(|id| &program.functions[id]) else {
        return Ok(());
    };
    let globals = program
        .globals
        .iter()
        .map(|(id, GlobalInit { value, .. })| (id, value.clone()))
        .collect();
    let mut machine = Machine {
        functions: Some(&program.functions),
        types: &program.types,
        drop_fns: Some(&program.drop_fns),
        display_fns: Some(&program.display_fns),
        has_drops: HashMap::new(),
        scratch: Vec::new(),
        globals,
        stack: Vec::new(),
        out,
        err,
    };
    machine.run(entry)?;
    // Globals are destroyed after `main` returns, in order.
    for (id, _) in program.globals.iter() {
        machine.destroy(&Location {
            root: Root::Global(id),
            path: Vec::new(),
        })?;
    }
    Ok(())
}

/// Evaluates a body without calls or globals, such as a constant initializer.
pub(crate) fn eval_body(body: &Body, types: &Types) -> Result<Value, InterpretError> {
    let mut sink = std::io::sink();
    let mut sink_err = std::io::sink();
    let mut machine = Machine {
        functions: None,
        types,
        drop_fns: None,
        display_fns: None,
        has_drops: HashMap::new(),
        scratch: Vec::new(),
        globals: ArenaMap::default(),
        stack: Vec::new(),
        out: &mut sink,
        err: &mut sink_err,
    };
    machine.run(body)
}

impl<'p> Machine<'p, '_> {
    fn run(&mut self, entry: &'p Body) -> Result<Value, InterpretError> {
        self.stack.push(Frame {
            body: entry,
            locals: ArenaMap::default(),
            block: entry.entry,
            return_to: None,
            error_to: None,
            env: None,
        });
        self.run_frames(0)
    }

    /// Runs the frames above `base` until the lowest of them returns; returns its result.
    fn run_frames(&mut self, base: usize) -> Result<Value, InterpretError> {
        loop {
            let frame_index = self.stack.len() - 1;
            let body = self.stack[frame_index].body;
            let block = &body.blocks[self.stack[frame_index].block];
            for statement in &block.statements {
                self.execute(frame_index, statement)?;
            }
            match &block.terminator {
                Terminator::Goto(target) => self.stack[frame_index].block = *target,
                Terminator::If {
                    cond,
                    then_block,
                    else_block,
                } => {
                    let cond = self
                        .read(frame_index, cond)
                        .as_bool()
                        .expect("conditions are bool");
                    self.stack[frame_index].block = if cond { *then_block } else { *else_block };
                }
                Terminator::Call {
                    func,
                    args,
                    destination,
                    target,
                    on_error,
                } => self.call(
                    frame_index,
                    func,
                    args,
                    (destination, *target),
                    on_error.as_ref(),
                )?,
                Terminator::Raise(error) => {
                    let error = self.read(frame_index, error);
                    let finished = self.stack.pop().expect("a frame is running");
                    if self.stack.len() == base {
                        return Err(InterpretError::Uncaught {
                            errors: error.error_chain(),
                        });
                    }
                    let (place, block) = finished
                        .error_to
                        .expect("a function that raises has an error target");
                    let caller = self.stack.len() - 1;
                    let location = self.locate(caller, &place);
                    self.store(&location, error);
                    self.stack[caller].block = block;
                }
                Terminator::Return => {
                    let finished = self.stack.pop().expect("a frame is running");
                    let value = match finished.locals.get(finished.body.return_local) {
                        Some(Slot::Value(value)) => value.clone(),
                        _ => Value::Nothing,
                    };
                    if self.stack.len() == base {
                        return Ok(value);
                    }
                    let (destination, target) = finished
                        .return_to
                        .expect("a function that returns has a return target");
                    let caller = self.stack.len() - 1;
                    let location = self.locate(caller, &destination);
                    self.store(&location, value);
                    self.stack[caller].block = target;
                }
                Terminator::Panic {
                    kind,
                    message,
                    span,
                } => {
                    let text = if message.is_empty() {
                        kind.message().to_owned()
                    } else {
                        let parts = self.format_parts(frame_index, message)?;
                        format!("{}{parts}", kind.message_prefix())
                    };
                    return Err(InterpretError::Panic {
                        kind: *kind,
                        message: text,
                        span: *span,
                    });
                }
                Terminator::Unreachable => unreachable!("reached a block marked unreachable"),
            }
        }
    }

    // ----- Destruction --------------------------------------------------------------------

    /// Whether destroying a value of type `ty` runs `drop` functions.
    fn has_drops(&mut self, ty: Ty) -> bool {
        if self.drop_fns.is_none() {
            return false;
        }
        if let Some(&known) = self.has_drops.get(&ty) {
            return known;
        }
        // What a function value captured does not show in its type.
        if self.types.holds_functions(ty) {
            self.has_drops.insert(ty, true);
            return true;
        }
        let has = !self.types.user_drops(ty).is_empty();
        self.has_drops.insert(ty, has);
        has
    }

    /// Runs the `drop` functions of the value at `location` and of the values inside it:
    /// a value's own first, then those of its parts in order. The value stays in place.
    fn destroy(&mut self, location: &Location) -> Result<(), InterpretError> {
        let ty = self.value_ref(location).ty();
        if !self.has_drops(ty) {
            return Ok(());
        }
        if let Some(&func) = self.drop_fns.and_then(|fns| fns.get(&ty)) {
            self.call_now(func, location.clone())?;
        }
        let steps: Vec<Step> = match self.value_ref(location) {
            Value::Struct { fields, .. } => (0..fields.len())
                .map(|i| Step::Field(u32::try_from(i).expect("few fields")))
                .collect(),
            Value::Variant {
                variant, fields, ..
            } => (0..fields.len())
                .map(|i| Step::VariantField(*variant, u32::try_from(i).expect("few fields")))
                .collect(),
            Value::Boxed(_) => vec![Step::Deref],
            Value::List { elements, .. } => (0..elements.len()).map(Step::Element).collect(),
            Value::Map { entries, .. } => (0..entries.len())
                .flat_map(|i| [Step::Key(i), Step::Value(i)])
                .collect(),
            // Each copy of a function value is given up as it is destroyed (the value left in
            // its place is nothing), and the last one destroys the captured values.
            Value::Function(_) => {
                let taken = std::mem::replace(self.value_mut(location), Value::Nothing);
                if let Value::Function(function) = taken
                    && let Ok(function) = Arc::try_unwrap(function)
                {
                    for capture in function.captures {
                        self.destroy_value(capture)?;
                    }
                }
                Vec::new()
            }
            _ => Vec::new(),
        };
        for step in steps {
            let mut part = location.clone();
            part.path.push(step);
            self.destroy(&part)?;
        }
        Ok(())
    }

    /// Destroys a value that is not in any variable.
    fn destroy_value(&mut self, value: Value) -> Result<(), InterpretError> {
        if !self.has_drops(value.ty()) {
            return Ok(());
        }
        let index = self.scratch.len();
        self.scratch.push(Some(value));
        let location = Location {
            root: Root::Scratch(index),
            path: Vec::new(),
        };
        let result = self.destroy(&location);
        self.scratch.pop();
        result
    }

    /// Calls the `drop` function `func` on the value at `location`, and runs it to the end.
    fn call_now(&mut self, func: InstanceId, location: Location) -> Result<(), InterpretError> {
        let callee = &self
            .functions
            .expect("only programs destroy values with `drop`")[func];
        let mut locals = ArenaMap::default();
        locals.insert(callee.params[0], Slot::Ref(location));
        self.run_call(callee, locals)
    }

    /// Calls `callee` with its parameters in `locals`, and runs it to the end.
    fn run_call(
        &mut self,
        callee: &'p Body,
        locals: ArenaMap<LocalId, Slot>,
    ) -> Result<(), InterpretError> {
        if self.stack.len() >= MAX_CALL_DEPTH {
            return Err(InterpretError::Panic {
                kind: PanicKind::StackOverflow,
                message: PanicKind::StackOverflow.message().to_owned(),
                span: callee.span,
            });
        }
        let base = self.stack.len();
        self.stack.push(Frame {
            body: callee,
            locals,
            block: callee.entry,
            return_to: None,
            error_to: None,
            env: None,
        });
        self.run_frames(base).map(|_| ())
    }

    /// Pushes the frame of a called function.
    fn call(
        &mut self,
        frame_index: usize,
        func: &CallTarget,
        args: &[CallArg],
        (destination, target): (&Place, Option<BlockId>),
        on_error: Option<&ErrorTarget>,
    ) -> Result<(), InterpretError> {
        let (func, env) = match func {
            CallTarget::Direct(func) => (*func, None),
            CallTarget::Value(place) => {
                let Value::Function(value) = self.place_value(frame_index, place) else {
                    unreachable!("a call of a value that is not a function");
                };
                (value.func, Some(value.clone()))
            }
        };
        let callee = &self.functions.expect("calls only occur in programs")[func];
        if self.stack.len() >= MAX_CALL_DEPTH {
            return Err(InterpretError::Panic {
                kind: PanicKind::StackOverflow,
                message: PanicKind::StackOverflow.message().to_owned(),
                span: callee.span,
            });
        }
        let mut locals = ArenaMap::default();
        for (&param, arg) in callee.params.iter().zip(args) {
            let slot = match arg {
                CallArg::Value(operand) => Slot::Value(self.read(frame_index, operand)),
                CallArg::Ref { place, .. } => Slot::Ref(self.locate(frame_index, place)),
            };
            debug_assert_eq!(
                matches!(slot, Slot::Ref(_)),
                matches!(callee.locals[param].mode, LocalMode::Ref { .. }),
                "argument passing does not match the parameter's mode"
            );
            locals.insert(param, slot);
        }
        self.stack.push(Frame {
            body: callee,
            locals,
            block: callee.entry,
            return_to: target.map(|target| (destination.clone(), target)),
            error_to: on_error.map(|e| (e.place.clone(), e.block)),
            env,
        });
        Ok(())
    }

    fn execute(&mut self, frame: usize, statement: &Statement) -> Result<(), InterpretError> {
        match statement {
            Statement::Assign { place, value, span } => {
                let value = self.evaluate(frame, value, *span)?;
                let location = self.locate(frame, place);
                self.store(&location, value);
            }
            Statement::Drop { place, flag } => {
                if let Some(flag) = flag {
                    let set = self.read(
                        frame,
                        &Operand::Copy {
                            place: Place::Local(*flag),
                            span: Span::empty(0),
                        },
                    );
                    if set.as_bool() != Some(true) {
                        return Ok(());
                    }
                }
                let location = self.locate(frame, place);
                self.destroy(&location)?;
                let dropped = self.take(&location);
                debug_assert!(
                    dropped.is_some(),
                    "dropped a value that is not there: {place:?} in `{}`",
                    self.stack[frame].body.name
                );
            }
            Statement::BindRef { local, place } => {
                let location = self.locate(frame, place);
                self.stack[frame].locals.insert(*local, Slot::Ref(location));
            }
            Statement::MarkMoved(local) => {
                let removed = self.stack[frame].locals.remove(*local);
                debug_assert!(
                    matches!(removed, Some(Slot::Value(_))),
                    "marked a local without a value as moved: {local:?}"
                );
            }
            Statement::ListPush { list, value } => {
                let value = self.read(frame, value);
                self.list_mut(frame, list).push(value);
            }
            Statement::ListInsert { list, index, value } => {
                let index = int_position(&self.read(frame, index));
                let value = self.read(frame, value);
                self.list_mut(frame, list).insert(index, value);
            }
            Statement::ListSwap {
                list,
                first,
                second,
            } => {
                let first = int_position(&self.read(frame, first));
                let second = int_position(&self.read(frame, second));
                self.list_mut(frame, list).swap(first, second);
            }
            Statement::Clear(place) => {
                let location = self.locate(frame, place);
                self.destroy(&location)?;
                match self.value_mut(&location) {
                    Value::List { elements, .. } => elements.clear(),
                    Value::Map { entries, .. } => entries.clear(),
                    other => unreachable!("clear of {other:?}"),
                }
            }
            Statement::Append { target, parts } => {
                let text = self.format_parts(frame, parts)?;
                let location = self.locate(frame, target);
                let Value::Str(existing) = self.value_mut(&location) else {
                    unreachable!("appended to a value that is not text");
                };
                *existing = format!("{existing}{text}").into();
            }
            Statement::Print {
                parts,
                stream,
                newline,
            } => {
                // Each piece is written as soon as it is known, as compiled programs do: a
                // `fmt` function that panics leaves the pieces before it written.
                for part in parts {
                    let text = self.format_parts(frame, std::slice::from_ref(part))?;
                    self.write(*stream, &text);
                }
                if *newline {
                    self.write(*stream, "\n");
                }
            }
        }
        Ok(())
    }

    /// Computes the value of an rvalue.
    fn evaluate(
        &mut self,
        frame: usize,
        value: &Rvalue,
        span: Span,
    ) -> Result<Value, InterpretError> {
        let panic = |kind: PanicKind| InterpretError::Panic {
            kind,
            message: kind.message().to_owned(),
            span,
        };
        Ok(match value {
            Rvalue::Use(operand) => self.read(frame, operand),
            Rvalue::Unary { op, operand } => {
                unary(*op, &self.read(frame, operand)).map_err(panic)?
            }
            Rvalue::Binary { op, lhs, rhs } => {
                let (lhs, rhs) = (self.read(frame, lhs), self.read(frame, rhs));
                binary(*op, &lhs, &rhs).map_err(panic)?
            }
            Rvalue::Cast { kind, operand, to } => {
                cast(*kind, &self.read(frame, operand), *to).map_err(panic)?
            }
            Rvalue::Interpolate(parts) => Value::Str(self.format_parts(frame, parts)?.into()),
            Rvalue::Clone(place) => {
                let location = self.locate(frame, place);
                self.load(&location)
            }
            Rvalue::Struct { ty, fields } => {
                let shape = self
                    .types
                    .struct_shape(*ty)
                    .expect("struct literals have struct types");
                let fields = fields.iter().map(|field| self.read(frame, field)).collect();
                Value::Struct { shape, fields }
            }
            Rvalue::Variant {
                ty,
                variant,
                fields,
            } => {
                let shape = self
                    .types
                    .enum_shape(*ty)
                    .expect("variants have enum or option types");
                let fields = fields.iter().map(|field| self.read(frame, field)).collect();
                Value::Variant {
                    shape,
                    variant: *variant,
                    fields,
                }
            }
            Rvalue::Discriminant(place) => {
                let location = self.locate(frame, place);
                match self.load(&location) {
                    Value::Variant { variant, .. } => Value::Int {
                        value: i128::from(variant),
                        ty: pika_hir::IntTy::U32,
                    },
                    other => unreachable!("discriminant of {other:?}"),
                }
            }
            Rvalue::BoxNew(operand) => Value::Boxed(Box::new(self.read(frame, operand))),
            Rvalue::Unbox(operand) => match self.read(frame, operand) {
                Value::Boxed(value) => *value,
                other => unreachable!("unbox of {other:?}"),
            },
            Rvalue::List { .. }
            | Rvalue::Map { .. }
            | Rvalue::Len(_)
            | Rvalue::MapFind { .. }
            | Rvalue::ListPop(_)
            | Rvalue::ListRemove { .. }
            | Rvalue::MapInsert { .. }
            | Rvalue::MapRemove { .. }
            | Rvalue::MapKeys(_)
            | Rvalue::MapValues(_)
            | Rvalue::Contains { .. } => self.evaluate_collection(frame, value)?,
            Rvalue::Closure { ty, func, captures } => {
                let captures = captures.iter().map(|c| self.read(frame, c)).collect();
                Value::Function(Arc::new(FunctionValue {
                    ty: *ty,
                    func: *func,
                    captures,
                }))
            }
            Rvalue::StringLen(operand) => match self.read(frame, operand) {
                Value::Str(text) => Value::Int {
                    value: i128::try_from(text.len()).expect("lengths fit in i128"),
                    ty: pika_hir::IntTy::I64,
                },
                other => unreachable!("length of {other:?}"),
            },
            Rvalue::Intrinsic { intrinsic, args } => {
                let values: Vec<Value> = args.iter().map(|arg| self.read(frame, arg)).collect();
                let args: Vec<Arg<'_>> = values.iter().map(intrinsic_arg).collect();
                let mut io = MachineIo {
                    out: &mut *self.out,
                    err: &mut *self.err,
                };
                intrinsic_value(intrinsic.call(&args, &mut io))
            }
        })
    }

    /// Computes the value of an rvalue of a list, map or set.
    /// A map or set literal.
    fn evaluate_map(
        &mut self,
        frame: usize,
        ty: Ty,
        entries: &[(Operand, Operand)],
    ) -> Result<Value, InterpretError> {
        let mut map: Vec<(Value, Value)> = Vec::with_capacity(entries.len());
        for (key, value) in entries {
            let (key, value) = (self.read(frame, key), self.read(frame, value));
            match Value::find_key(&map, &key) {
                // A repeated key keeps its first position and its last value.
                Some(index) => {
                    let old = std::mem::replace(&mut map[index].1, value);
                    self.destroy_value(old)?;
                    self.destroy_value(key)?;
                }
                None => map.push((key, value)),
            }
        }
        Ok(Value::Map { ty, entries: map })
    }

    fn evaluate_collection(
        &mut self,
        frame: usize,
        value: &Rvalue,
    ) -> Result<Value, InterpretError> {
        Ok(match value {
            Rvalue::List { ty, elements } => Value::List {
                ty: *ty,
                elements: elements.iter().map(|e| self.read(frame, e)).collect(),
            },
            Rvalue::Map { ty, entries } => self.evaluate_map(frame, *ty, entries)?,
            Rvalue::Len(place) => match self.place_value(frame, place) {
                Value::List { elements, .. } => int_value(elements.len()),
                Value::Map { entries, .. } => int_value(entries.len()),
                other => unreachable!("length of {other:?}"),
            },
            Rvalue::MapFind { map, key } => {
                let key = self.read(frame, key);
                let Value::Map { entries, .. } = self.place_value(frame, map) else {
                    unreachable!("not a map");
                };
                match Value::find_key(entries, &key) {
                    Some(index) => int_value(index),
                    None => Value::Int {
                        value: -1,
                        ty: pika_hir::IntTy::I64,
                    },
                }
            }
            Rvalue::ListPop(list) => {
                let ty = self.place_value(frame, list).ty();
                let popped = self.list_mut(frame, list).pop();
                self.option(option_of_element(ty), popped)
            }
            Rvalue::ListRemove { list, index } => {
                let index = int_position(&self.read(frame, index));
                self.list_mut(frame, list).remove(index)
            }
            Rvalue::MapInsert { map, key, value } => {
                let ty = self.place_value(frame, map).ty();
                let (key, value) = (self.read(frame, key), self.read(frame, value));
                let entries = self.entries_mut(frame, map);
                let old = if let Some(index) = Value::find_key(entries, &key) {
                    let old = std::mem::replace(&mut entries[index].1, value);
                    // The map keeps its key.
                    self.destroy_value(key)?;
                    Some(old)
                } else {
                    entries.push((key, value));
                    None
                };
                self.option(option_of_value(ty), old)
            }
            Rvalue::MapRemove { map, key } => {
                let ty = self.place_value(frame, map).ty();
                let key = self.read(frame, key);
                let entries = self.entries_mut(frame, map);
                let removed = Value::find_key(entries, &key).map(|index| entries.remove(index));
                // The map's key is destroyed; its value is returned.
                let removed = match removed {
                    Some((removed_key, value)) => {
                        self.destroy_value(removed_key)?;
                        Some(value)
                    }
                    None => None,
                };
                self.option(option_of_value(ty), removed)
            }
            Rvalue::MapKeys(map) | Rvalue::MapValues(map) => {
                let keys = matches!(value, Rvalue::MapKeys(_));
                match self.place_value(frame, map) {
                    Value::Map {
                        ty: Ty::Map(map_ty),
                        entries,
                    } => Value::List {
                        ty: Ty::list(if keys { map_ty.key } else { map_ty.value }),
                        elements: entries
                            .iter()
                            .map(|(k, v)| if keys { k.clone() } else { v.clone() })
                            .collect(),
                    },
                    other => unreachable!("keys of {other:?}"),
                }
            }
            Rvalue::Contains { collection, value } => {
                let value = self.read(frame, value);
                Value::Bool(match self.place_value(frame, collection) {
                    Value::List { elements, .. } => elements.iter().any(|e| {
                        matches!(
                            binary(crate::BinaryOp::Eq, e, &value),
                            Ok(Value::Bool(true))
                        )
                    }),
                    Value::Map { entries, .. } => Value::find_key(entries, &value).is_some(),
                    other => unreachable!("membership in {other:?}"),
                })
            }
            other => unreachable!("not a collection rvalue: {other:?}"),
        })
    }

    /// Writes text to an output stream. Like compiled programs, interpreted programs ignore
    /// output errors.
    fn write(&mut self, stream: Stream, text: &str) {
        let writer = match stream {
            Stream::Stdout => &mut *self.out,
            Stream::Stderr => &mut *self.err,
        };
        let _ = writer.write_all(text.as_bytes());
    }

    /// The text of pieces to print or append.
    fn format_parts(
        &mut self,
        frame: usize,
        parts: &[PrintPart],
    ) -> Result<String, InterpretError> {
        let mut text = String::new();
        for part in parts {
            match part {
                PrintPart::Text(piece) => text.push_str(piece),
                PrintPart::Value(operand) => {
                    let value = self.read(frame, operand);
                    text.push_str(&self.display(&value)?);
                }
            }
        }
        Ok(text)
    }

    /// The text of a value, running the `fmt` functions of the types that have one.
    fn display(&mut self, value: &Value) -> Result<String, InterpretError> {
        if self.display_fns.is_none_or(HashMap::is_empty) {
            return Ok(value.display());
        }
        let mut failed = None;
        let text = value.display_with(&mut |part| {
            let func = *self.display_fns?.get(&part.ty())?;
            match self.run_fmt(func, part.clone()) {
                Ok(text) => Some(text),
                Err(error) => {
                    failed.get_or_insert(error);
                    Some(String::new())
                }
            }
        });
        failed.map_or(Ok(text), Err)
    }

    /// Runs the `fmt` function `func` on `value`, and returns the text it wrote.
    fn run_fmt(&mut self, func: InstanceId, value: Value) -> Result<String, InterpretError> {
        let callee = &self.functions.expect("only programs have `fmt` functions")[func];
        let base_scratch = self.scratch.len();
        let this = match callee.locals[callee.params[0]].mode {
            LocalMode::Ref { .. } => {
                self.scratch.push(Some(value));
                Slot::Ref(Location {
                    root: Root::Scratch(base_scratch),
                    path: Vec::new(),
                })
            }
            LocalMode::Value => Slot::Value(value),
        };
        let out = self.scratch.len();
        self.scratch.push(Some(Value::Str("".into())));
        let mut locals = ArenaMap::default();
        locals.insert(callee.params[0], this);
        locals.insert(
            callee.params[1],
            Slot::Ref(Location {
                root: Root::Scratch(out),
                path: Vec::new(),
            }),
        );
        let result = self.run_call(callee, locals);
        let text = match self.scratch[out].take() {
            Some(Value::Str(text)) => text.to_string(),
            _ => String::new(),
        };
        self.scratch.truncate(base_scratch);
        result.map(|()| text)
    }

    /// The storage location of a place, following references of borrowed parameters.
    fn locate(&self, frame: usize, place: &Place) -> Location {
        match place {
            Place::Global(global) => Location {
                root: Root::Global(*global),
                path: Vec::new(),
            },
            Place::Local(local) => match self.stack[frame].locals.get(*local) {
                Some(Slot::Ref(location)) => location.clone(),
                _ => Location {
                    root: Root::Local {
                        frame,
                        local: *local,
                    },
                    path: Vec::new(),
                },
            },
            Place::Capture(index) => Location {
                root: Root::Capture {
                    frame,
                    index: *index,
                },
                path: Vec::new(),
            },
            Place::Field(base, index) => self.locate_part(frame, base, Step::Field(*index)),
            Place::VariantField(base, variant, index) => {
                self.locate_part(frame, base, Step::VariantField(*variant, *index))
            }
            Place::Deref(base) => self.locate_part(frame, base, Step::Deref),
            Place::Index(base, index) => {
                let index = self.position(frame, *index);
                self.locate_part(frame, base, Step::Element(index))
            }
            Place::MapKey(base, index) => {
                let index = self.position(frame, *index);
                self.locate_part(frame, base, Step::Key(index))
            }
            Place::MapValue(base, index) => {
                let index = self.position(frame, *index);
                self.locate_part(frame, base, Step::Value(index))
            }
        }
    }

    fn locate_part(&self, frame: usize, base: &Place, step: Step) -> Location {
        let mut location = self.locate(frame, base);
        location.path.push(step);
        location
    }

    /// The position held by an `int` local, checked to be within bounds before.
    fn position(&self, frame: usize, local: LocalId) -> usize {
        int_position(&self.load(&self.locate(frame, &Place::Local(local))))
    }

    /// The value of the variable holding a location.
    fn root_value(&self, root: Root) -> &Value {
        match root {
            Root::Global(global) => self.globals.get(global).expect("globals are initialized"),
            Root::Capture { frame, index } => {
                let env = self.stack[frame]
                    .env
                    .as_ref()
                    .expect("captures are read in the bodies of function values");
                &env.captures[index as usize]
            }
            Root::Scratch(index) => self.scratch[index]
                .as_ref()
                .expect("scratch values are set"),
            Root::Local { frame, local } => match self.stack[frame].locals.get(local) {
                Some(Slot::Value(value)) => value,
                // Ruled out by the ownership analysis.
                other => unreachable!("read of an unassigned or moved local {local:?}: {other:?}"),
            },
        }
    }

    fn load(&self, location: &Location) -> Value {
        self.value_ref(location).clone()
    }

    /// The value at a location that holds one.
    fn value_ref(&self, location: &Location) -> &Value {
        location
            .path
            .iter()
            .fold(self.root_value(location.root), |value, &step| {
                field(value, step)
            })
    }

    /// The value in a place, without copying it.
    fn place_value(&self, frame: usize, place: &Place) -> &Value {
        self.value_ref(&self.locate(frame, place))
    }

    fn store(&mut self, location: &Location, value: Value) {
        if location.path.is_empty() {
            match location.root {
                Root::Global(global) => {
                    self.globals.insert(global, value);
                }
                Root::Scratch(index) => self.scratch[index] = Some(value),
                Root::Capture { .. } => unreachable!("captured values are not assigned"),
                Root::Local { frame, local } => {
                    self.stack[frame].locals.insert(local, Slot::Value(value));
                }
            }
            return;
        }
        *self.value_mut(location) = value;
    }

    /// The value at a location that holds one, to modify.
    fn value_mut(&mut self, location: &Location) -> &mut Value {
        let root = match location.root {
            Root::Global(global) => self
                .globals
                .get_mut(global)
                .expect("globals are initialized"),
            Root::Scratch(index) => self.scratch[index]
                .as_mut()
                .expect("scratch values are set"),
            Root::Capture { .. } => unreachable!("captured values are not modified"),
            Root::Local { frame, local } => match self.stack[frame].locals.get_mut(local) {
                Some(Slot::Value(value)) => value,
                other => unreachable!("modified an unassigned local: {other:?}"),
            },
        };
        location
            .path
            .iter()
            .fold(root, |value, &step| field_mut(value, step))
    }

    /// The elements of the list in a place, to modify.
    fn list_mut(&mut self, frame: usize, place: &Place) -> &mut Vec<Value> {
        let location = self.locate(frame, place);
        match self.value_mut(&location) {
            Value::List { elements, .. } => elements,
            other => unreachable!("not a list: {other:?}"),
        }
    }

    /// The entries of the map or set in a place, to modify.
    fn entries_mut(&mut self, frame: usize, place: &Place) -> &mut Vec<(Value, Value)> {
        let location = self.locate(frame, place);
        match self.value_mut(&location) {
            Value::Map { entries, .. } => entries,
            other => unreachable!("not a map: {other:?}"),
        }
    }

    /// `[some value]` or `none`, of option type `option`.
    fn option(&self, option: Ty, value: Option<Value>) -> Value {
        let shape = self.types.enum_shape(option).expect("options have shapes");
        match value {
            Some(value) => Value::Variant {
                shape,
                variant: 1,
                fields: vec![value],
            },
            None => Value::Variant {
                shape,
                variant: 0,
                fields: Vec::new(),
            },
        }
    }

    /// Removes the value from a location, which is uninitialized afterwards. A field keeps
    /// its value until it is assigned again, which happens at once.
    fn take(&mut self, location: &Location) -> Option<Value> {
        if !location.path.is_empty() {
            return Some(self.load(location));
        }
        match location.root {
            // Globals are only dropped to be replaced at once.
            Root::Global(global) => self.globals.get(global).cloned(),
            Root::Scratch(index) => self.scratch[index].take(),
            Root::Capture { .. } => unreachable!("captured values are not moved"),
            Root::Local { frame, local } => match self.stack[frame].locals.remove(local) {
                Some(Slot::Value(value)) => Some(value),
                Some(Slot::Ref(_)) => unreachable!("references are followed by `locate`"),
                None => None,
            },
        }
    }

    fn read(&mut self, frame: usize, operand: &Operand) -> Value {
        match operand {
            Operand::Const(value) => value.clone(),
            Operand::Copy { place, .. } => {
                let location = self.locate(frame, place);
                self.load(&location)
            }
            Operand::Move { place, .. } => {
                let location = self.locate(frame, place);
                self.take(&location)
                    .expect("moved a value that is not there")
            }
        }
    }
}

/// An `int` position, checked to be within bounds before.
fn int_position(value: &Value) -> usize {
    match value {
        Value::Int { value, .. } => usize::try_from(*value).expect("positions are checked"),
        other => unreachable!("a position that is not an integer: {other:?}"),
    }
}

/// `T?` for the elements of list type `list`.
fn option_of_element(list: Ty) -> Ty {
    match list {
        Ty::List(element) => Ty::option(*element),
        other => unreachable!("not a list: {other}"),
    }
}

/// `V?` for the values of map or set type `map` (`nothing?` for a set).
fn option_of_value(map: Ty) -> Ty {
    match map {
        Ty::Map(map) => Ty::option(map.value),
        Ty::Set(_) => Ty::option(Ty::Nothing),
        other => unreachable!("not a map: {other}"),
    }
}

/// An `int` value.
fn int_value(value: usize) -> Value {
    Value::Int {
        value: i128::try_from(value).expect("lengths fit in i128"),
        ty: pika_hir::IntTy::I64,
    }
}

/// The part of a value that a step leads to.
fn field(value: &Value, step: Step) -> &Value {
    match (value, step) {
        (Value::Struct { fields, .. }, Step::Field(index)) => &fields[index as usize],
        (
            Value::Variant {
                variant, fields, ..
            },
            Step::VariantField(expected, index),
        ) => {
            debug_assert_eq!(*variant, expected, "a field of another variant");
            &fields[index as usize]
        }
        (Value::Boxed(inner), Step::Deref) => inner,
        (Value::List { elements, .. }, Step::Element(index)) => &elements[index],
        (Value::Map { entries, .. }, Step::Key(index)) => &entries[index].0,
        (Value::Map { entries, .. }, Step::Value(index)) => &entries[index].1,
        (other, step) => unreachable!("{step:?} of {other:?}"),
    }
}

fn field_mut(value: &mut Value, step: Step) -> &mut Value {
    match (value, step) {
        (Value::Struct { fields, .. }, Step::Field(index)) => &mut fields[index as usize],
        (
            Value::Variant {
                variant, fields, ..
            },
            Step::VariantField(expected, index),
        ) => {
            debug_assert_eq!(*variant, expected, "a field of another variant");
            &mut fields[index as usize]
        }
        (Value::Boxed(inner), Step::Deref) => inner,
        (Value::List { elements, .. }, Step::Element(index)) => &mut elements[index],
        (Value::Map { entries, .. }, Step::Key(index)) => &mut entries[index].0,
        (Value::Map { entries, .. }, Step::Value(index)) => &mut entries[index].1,
        (other, step) => unreachable!("{step:?} of {other:?}"),
    }
}
