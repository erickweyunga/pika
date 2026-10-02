//! Translation of MIR bodies into Cranelift IR.

use std::collections::HashMap;

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{
    Block, FuncRef, Function, GlobalValue, InstBuilder, MemFlagsData, StackSlot, StackSlotData,
    StackSlotKind, TrapCode, Type, Value, types,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::JITModule;
use cranelift_module::{DataId, FuncId, Module};
use la_arena::ArenaMap;
use pika_diagnostics::{SourceMap, Span};
use pika_hir::{FloatTy, GlobalId, IntTy};
use pika_mir::{
    BinaryOp, BlockId, Body, CallArg, CallTarget, CastKind, ErrorTarget, Intrinsic, LocalId,
    LocalMode, Operand, PanicKind, Place, PrintPart, Program, Rvalue, Statement, Stream,
    Terminator, UnaryOp, Value as MirValue, kind_ty,
};
use pika_types::Ty;

use crate::glue::{EQUAL, Emitter, GREATER, LESS, Loaded};
use crate::repr::{self, LEN_OFFSET, Repr, STRING_SIZE};
use crate::{CodegenError, ModuleCtx};

/// Where a local slot lives.
#[derive(Clone, Copy)]
enum Storage {
    /// An SSA variable holding a scalar.
    Var(Variable),
    /// A stack slot: for values in memory, and scalars whose address is taken.
    Slot(StackSlot),
    /// A variable holding the address of the value (a borrowed parameter).
    Ref(Variable),
}

/// Translates one MIR body into `func`.
#[allow(
    clippy::too_many_arguments,
    reason = "the translation needs all of its context"
)]
pub(crate) fn translate_body(
    module: &mut JITModule,
    ctx: &mut ModuleCtx,
    program: &Program,
    body: &Body,
    func: &mut Function,
    builder_context: &mut FunctionBuilderContext,
    sources: &SourceMap,
) -> Result<(), CodegenError> {
    let pointer = module.target_config().pointer_type();
    let builder = FunctionBuilder::new(func, builder_context);
    let mut translator = Translator {
        builder,
        module,
        ctx,
        program,
        body,
        blocks: ArenaMap::default(),
        storage: ArenaMap::default(),
        result_pointer: None,
        error_pointer: None,
        env_pointer: None,
        sources,
        pointer,
        func_refs: HashMap::new(),
        data_values: HashMap::new(),
    };
    translator.translate()?;
    let frontend_config = translator.module.target_config();
    translator.builder.seal_all_blocks();
    translator.builder.finalize(frontend_config);
    Ok(())
}

struct Translator<'a, 'f> {
    builder: FunctionBuilder<'f>,
    module: &'a mut JITModule,
    ctx: &'a mut ModuleCtx,
    program: &'a Program,
    body: &'a Body,
    blocks: ArenaMap<BlockId, Block>,
    storage: ArenaMap<LocalId, Storage>,
    /// The destination of a result returned in memory.
    result_pointer: Option<Variable>,
    /// In a function that raises, the `Error?` where its error goes.
    error_pointer: Option<Variable>,
    /// In the body of a function value: its object, which holds the captured values.
    env_pointer: Option<Variable>,
    sources: &'a SourceMap,
    pointer: Type,
    func_refs: HashMap<FuncId, FuncRef>,
    data_values: HashMap<DataId, GlobalValue>,
}

/// The locals whose address is passed to a call.
fn address_taken(body: &Body) -> ArenaMap<LocalId, ()> {
    let mut taken = ArenaMap::default();
    for (_, block) in body.blocks.iter() {
        if let Terminator::Call { args, .. } = &block.terminator {
            for arg in args {
                if let CallArg::Ref { place, .. } = arg
                    && let Some(local) = place.as_local()
                {
                    taken.insert(local, ());
                }
            }
        }
    }
    taken
}

impl<'f> Translator<'_, 'f> {
    fn translate(&mut self) -> Result<(), CodegenError> {
        for (id, _) in self.body.blocks.iter() {
            let block = self.builder.create_block();
            self.blocks.insert(id, block);
        }
        // A separate entry block, because the first MIR block could be a jump target.
        let entry = self.builder.create_block();
        self.builder.append_block_params_for_function_params(entry);
        self.builder.switch_to_block(entry);

        let taken = address_taken(self.body);
        for (local, decl) in self.body.locals.iter() {
            let storage = match (decl.mode, self.ctx.layouts.repr(decl.ty)) {
                (LocalMode::Ref { .. }, _) => {
                    let var = self.builder.declare_var(self.pointer);
                    let zero = self.zero(self.pointer);
                    self.builder.def_var(var, zero);
                    Storage::Ref(var)
                }
                (LocalMode::Value, Repr::Memory { size, align }) => {
                    Storage::Slot(self.stack_slot(size, align))
                }
                (LocalMode::Value, Repr::Scalar(ty)) if taken.get(local).is_some() => {
                    Storage::Slot(
                        self.stack_slot(ty.bytes(), u8::try_from(ty.bytes()).unwrap_or(8)),
                    )
                }
                (LocalMode::Value, Repr::Scalar(ty)) => {
                    let var = self.builder.declare_var(ty);
                    // Variables start at zero; the ownership analysis guarantees that user
                    // variables are assigned before they are read.
                    let zero = self.zero(ty);
                    self.builder.def_var(var, zero);
                    Storage::Var(var)
                }
                (LocalMode::Value, Repr::None) => continue,
            };
            self.storage.insert(local, storage);
        }

        let incoming: Vec<Value> = self.builder.block_params(entry).to_vec();
        let mut incoming = incoming.into_iter();
        if self.body.env.is_some() {
            let var = self.builder.declare_var(self.pointer);
            let value = incoming
                .next()
                .expect("the object of a function value is the first parameter");
            self.builder.def_var(var, value);
            self.env_pointer = Some(var);
        }
        if self.ctx.layouts.returns_in_memory(self.body) {
            let var = self.builder.declare_var(self.pointer);
            let value = incoming
                .next()
                .expect("the result pointer is the first parameter");
            self.builder.def_var(var, value);
            self.result_pointer = Some(var);
        }
        for &param in &self.body.params {
            let decl = &self.body.locals[param];
            let Some(storage) = self.storage.get(param).copied() else {
                continue;
            };
            let value = incoming
                .next()
                .expect("one parameter per represented argument");
            match (storage, self.ctx.layouts.repr(decl.ty)) {
                (Storage::Ref(var) | Storage::Var(var), _) => self.builder.def_var(var, value),
                // An owned value in memory: the caller passed the address of the value it
                // gave up, which is copied into this frame.
                (Storage::Slot(slot), Repr::Memory { size, align }) => {
                    let address = self.slot_address(slot);
                    self.copy_memory(address, value, size, align);
                }
                (Storage::Slot(slot), _) => {
                    let address = self.slot_address(slot);
                    self.builder
                        .ins()
                        .store(MemFlagsData::trusted(), value, address, 0);
                }
            }
        }
        if self.body.raises {
            let var = self.builder.declare_var(self.pointer);
            let value = incoming
                .next()
                .expect("the error pointer is the last parameter");
            self.builder.def_var(var, value);
            self.error_pointer = Some(var);
        }
        self.check_stack();
        self.builder.ins().jump(self.blocks[self.body.entry], &[]);

        for (id, block) in self.body.blocks.iter() {
            self.builder.switch_to_block(self.blocks[id]);
            for statement in &block.statements {
                self.statement(statement)?;
            }
            self.terminator(&block.terminator)?;
        }
        Ok(())
    }

    /// Panics with a stack overflow if the stack pointer is below the runtime's limit.
    fn check_stack(&mut self) {
        let stack_pointer = self.builder.ins().get_stack_pointer(self.pointer);
        let limit_address = self.data_address(self.ctx.stack_limit);
        let limit =
            self.builder
                .ins()
                .load(self.pointer, MemFlagsData::trusted(), limit_address, 0);
        let overflow = self
            .builder
            .ins()
            .icmp(IntCC::UnsignedLessThan, stack_pointer, limit);
        self.panic_if(overflow, PanicKind::StackOverflow, self.body.span);
    }

    // ----- Helpers -----------------------------------------------------------------------

    fn stack_slot(&mut self, size: u32, align: u8) -> StackSlot {
        let align_shift =
            u8::try_from(align.max(1).trailing_zeros()).expect("alignments are small");
        self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            size,
            align_shift,
        ))
    }

    fn slot_address(&mut self, slot: StackSlot) -> Value {
        self.builder.ins().stack_addr(self.pointer, slot, 0)
    }

    /// Emits operations on values of every type.
    fn emitter(&mut self) -> Emitter<'_, 'f> {
        Emitter {
            builder: &mut self.builder,
            module: &mut *self.module,
            ctx: &mut *self.ctx,
            pointer: self.pointer,
        }
    }

    fn copy_memory(&mut self, dest: Value, source: Value, size: u32, align: u8) {
        self.emitter().copy_memory(dest, source, size, align);
    }

    /// Copies the bytes of a value of type `ty` from `source` to `dest`.
    fn copy_value(&mut self, dest: Value, source: Value, ty: Ty) {
        self.emitter().copy_bytes(dest, source, ty);
    }

    /// `base + offset`
    fn offset_address(&mut self, base: Value, offset: u32) -> Value {
        self.emitter().offset(base, offset)
    }

    /// Stores a value of type `ty`, given as an operand, into the memory at `dest`.
    /// The address of an operand's value of type `ty`, in memory: a scalar is stored in a new
    /// stack slot.
    fn operand_in_memory(&mut self, operand: &Operand, ty: Ty) -> Result<Value, CodegenError> {
        match self.ctx.layouts.repr(ty) {
            Repr::Memory { .. } => self.operand_address(operand),
            Repr::Scalar(scalar) => {
                let slot =
                    self.stack_slot(scalar.bytes(), u8::try_from(scalar.bytes()).unwrap_or(8));
                let address = self.slot_address(slot);
                self.store_operand(address, operand, ty)?;
                Ok(address)
            }
            Repr::None => {
                let slot = self.stack_slot(1, 1);
                Ok(self.slot_address(slot))
            }
        }
    }

    fn store_operand(
        &mut self,
        dest: Value,
        operand: &Operand,
        ty: Ty,
    ) -> Result<(), CodegenError> {
        match self.ctx.layouts.repr(ty) {
            Repr::Scalar(_) => {
                if let Some(value) = self.operand(operand)? {
                    self.builder
                        .ins()
                        .store(MemFlagsData::trusted(), value, dest, 0);
                }
            }
            Repr::Memory { .. } => {
                let source = self.operand_address(operand)?;
                self.copy_value(dest, source, ty);
            }
            Repr::None => {}
        }
        Ok(())
    }

    /// The value of an operand: its scalar value, or its address if it is held in memory.
    fn load_operand(&mut self, operand: &Operand) -> Result<Loaded, CodegenError> {
        Ok(match self.ctx.layouts.repr(self.ty(operand)) {
            Repr::Scalar(_) => Loaded::Scalar(
                self.operand(operand)?
                    .ok_or_else(|| CodegenError("a scalar without a value".into()))?,
            ),
            Repr::Memory { .. } => Loaded::Address(self.operand_address(operand)?),
            Repr::None => Loaded::Nothing,
        })
    }

    fn zero(&mut self, ty: Type) -> Value {
        match ty {
            types::F32 => self.builder.ins().f32const(0.0),
            types::F64 => self.builder.ins().f64const(0.0),
            _ => self.builder.ins().iconst(ty, 0),
        }
    }

    /// An integer constant of type `ty` from its bit pattern.
    fn iconst(&mut self, ty: Type, bits: u64) -> Value {
        let masked = if ty.bits() == 64 {
            bits
        } else {
            bits & ((1u64 << ty.bits()) - 1)
        };
        #[allow(
            clippy::cast_possible_wrap,
            reason = "Cranelift takes the bit pattern as i64"
        )]
        self.builder.ins().iconst(ty, masked as i64)
    }

    #[allow(clippy::cast_sign_loss, reason = "converting to a bit pattern")]
    fn iconst_signed(&mut self, ty: Type, value: i64) -> Value {
        self.iconst(ty, value as u64)
    }

    fn func_ref(&mut self, func: FuncId) -> FuncRef {
        if let Some(&func_ref) = self.func_refs.get(&func) {
            return func_ref;
        }
        let func_ref = self.module.declare_func_in_func(func, self.builder.func);
        self.func_refs.insert(func, func_ref);
        func_ref
    }

    fn call_runtime(&mut self, func: FuncId, args: &[Value]) -> Option<Value> {
        let func_ref = self.func_ref(func);
        let call = self.builder.ins().call(func_ref, args);
        self.builder.inst_results(call).first().copied()
    }

    fn data_address(&mut self, data: DataId) -> Value {
        let global = if let Some(&global) = self.data_values.get(&data) {
            global
        } else {
            let global = self.module.declare_data_in_func(data, self.builder.func);
            self.data_values.insert(data, global);
            global
        };
        self.builder.ins().symbol_value(self.pointer, global)
    }

    fn global_address(&mut self, global: GlobalId) -> Value {
        let data = self.ctx.globals[global];
        self.data_address(data)
    }

    fn text_address(&mut self, text: &str) -> Result<(Value, Value), CodegenError> {
        let data = self.ctx.string_data(self.module, text)?;
        let address = self.data_address(data);
        let length = self.iconst(self.pointer, text.len() as u64);
        Ok((address, length))
    }

    /// The index of the file, the line and the column of `span`, for a panic location.
    fn location(&mut self, span: Span) -> [Value; 3] {
        let location = self.sources.locate(span);
        [
            u64::from(location.file.0),
            u64::from(location.line),
            u64::from(location.column),
        ]
        .map(|value| self.iconst(types::I32, value))
    }

    /// Panics with `kind` if `condition` (an `i8` boolean) is true.
    fn panic_if(&mut self, condition: Value, kind: PanicKind, span: Span) {
        let panic_block = self.builder.create_block();
        let continue_block = self.builder.create_block();
        self.builder.set_cold_block(panic_block);
        self.builder
            .ins()
            .brif(condition, panic_block, &[], continue_block, &[]);
        self.builder.switch_to_block(panic_block);
        self.panic(kind, span);
        self.builder.switch_to_block(continue_block);
    }

    /// Reports a runtime error with a fixed message; the block ends here.
    fn panic(&mut self, kind: PanicKind, span: Span) {
        let kind = self.iconst(types::I32, kind as u64);
        let [file, line, column] = self.location(span);
        self.call_runtime(self.ctx.runtime.panic, &[kind, file, line, column]);
        self.builder.ins().trap(TrapCode::unwrap_user(1));
    }

    fn ty(&self, operand: &Operand) -> Ty {
        self.program.operand_ty(self.body, operand)
    }

    fn place_ty(&self, place: &Place) -> Ty {
        self.program.place_ty(self.body, place)
    }

    // ----- Places and operands -----------------------------------------------------------

    /// The address of a place in memory.
    fn place_address(&mut self, place: &Place) -> Result<Value, CodegenError> {
        match place {
            Place::Global(global) => Ok(self.global_address(*global)),
            Place::Field(base, index) => {
                let base_ty = self.place_ty(base);
                let offset = self
                    .ctx
                    .layouts
                    .field_offset(base_ty, *index)
                    .ok_or_else(|| CodegenError(format!("no field {index} in {base_ty}")))?;
                let base = self.place_address(base)?;
                Ok(self.offset_address(base, offset))
            }
            Place::VariantField(base, variant, index) => {
                let base_ty = self.place_ty(base);
                let offset = self
                    .ctx
                    .layouts
                    .variant_field_offset(base_ty, *variant, *index)
                    .ok_or_else(|| {
                        CodegenError(format!(
                            "no field {index} of variant {variant} in {base_ty}"
                        ))
                    })?;
                let base = self.place_address(base)?;
                Ok(self.offset_address(base, offset))
            }
            // The value of a box is where the box points.
            Place::Deref(base) => self.read_scalar(base, self.pointer),
            Place::Index(base, index) => {
                let element = pika_mir::part_ty(self.place_ty(base), place);
                let list = self.place_address(base)?;
                let index = self.read_scalar(&Place::Local(*index), types::I64)?;
                Ok(self.emitter().element_addr(list, index, element))
            }
            Place::MapKey(base, index) | Place::MapValue(base, index) => {
                let entry = self.ctx.layouts.entry(self.place_ty(base));
                let map = self.place_address(base)?;
                let index = self.read_scalar(&Place::Local(*index), types::I64)?;
                let address = self.emitter().entry_addr(map, index, &entry);
                let offset = match place {
                    Place::MapKey(..) => entry.key_offset,
                    _ => entry.value_offset,
                };
                Ok(self.offset_address(address, offset))
            }
            &Place::Capture(index) => {
                let captures = self.body.env.clone().unwrap_or_default();
                let (offsets, _) = self.ctx.layouts.env_layout(&captures);
                let object = self.builder.use_var(
                    self.env_pointer
                        .expect("set in the bodies of function values"),
                );
                Ok(self.offset_address(object, offsets[index as usize]))
            }
            &Place::Local(local) => match self.storage.get(local).copied() {
                Some(Storage::Slot(slot)) => Ok(self.slot_address(slot)),
                Some(Storage::Ref(var)) => Ok(self.builder.use_var(var)),
                // A value without a representation has no bytes to point at: any non-null
                // address will do, as for the `self` of its `drop` function.
                None if self.ctx.layouts.repr(self.body.locals[local].ty) == Repr::None => {
                    let pointer = self.pointer;
                    Ok(self.emitter().iconst(pointer, 1))
                }
                Some(Storage::Var(_)) | None => Err(CodegenError(format!(
                    "the address of local {local:?} ({:?}) in `{}` is not available",
                    self.body.locals[local], self.body.name
                ))),
            },
        }
    }

    fn read_scalar(&mut self, place: &Place, ty: Type) -> Result<Value, CodegenError> {
        if let Place::Local(local) = *place
            && let Some(Storage::Var(var)) = self.storage.get(local).copied()
        {
            return Ok(self.builder.use_var(var));
        }
        let address = self.place_address(place)?;
        Ok(self
            .builder
            .ins()
            .load(ty, MemFlagsData::trusted(), address, 0))
    }

    fn write_scalar(&mut self, place: &Place, value: Value) -> Result<(), CodegenError> {
        if let Place::Local(local) = *place
            && let Some(Storage::Var(var)) = self.storage.get(local).copied()
        {
            self.builder.def_var(var, value);
            return Ok(());
        }
        let address = self.place_address(place)?;
        self.builder
            .ins()
            .store(MemFlagsData::trusted(), value, address, 0);
        Ok(())
    }

    /// The value of a scalar operand; `None` for a type without representation.
    fn operand(&mut self, operand: &Operand) -> Result<Option<Value>, CodegenError> {
        match operand {
            Operand::Const(value) => Ok(self.constant(value)),
            Operand::Copy { place, .. } | Operand::Move { place, .. } => {
                match self.ctx.layouts.repr(self.place_ty(place)) {
                    Repr::Scalar(ty) => self.read_scalar(place, ty).map(Some),
                    Repr::Memory { .. } | Repr::None => Ok(None),
                }
            }
        }
    }

    /// The address of an operand held in memory.
    fn operand_address(&mut self, operand: &Operand) -> Result<Value, CodegenError> {
        match operand {
            Operand::Copy { place, .. } | Operand::Move { place, .. } => self.place_address(place),
            Operand::Const(value) => self.constant_in_memory(value),
        }
    }

    /// A constant held in memory, in a new stack slot. Strings in it refer to static text
    /// (capacity 0), so they are never freed.
    fn constant_in_memory(&mut self, value: &MirValue) -> Result<Value, CodegenError> {
        let Repr::Memory { size, align } = self.ctx.layouts.repr(value.ty()) else {
            return Err(CodegenError(format!(
                "the constant {value:?} has no address"
            )));
        };
        let slot = self.stack_slot(size, align);
        let address = self.slot_address(slot);
        self.store_constant(address, value)?;
        Ok(address)
    }

    /// Writes a constant into the memory at `address`.
    fn store_constant(&mut self, address: Value, value: &MirValue) -> Result<(), CodegenError> {
        let flags = MemFlagsData::trusted();
        match value {
            MirValue::Str(text) => {
                let (text_address, length) = self.text_address(text)?;
                let zero = self.iconst(self.pointer, 0);
                self.builder.ins().store(flags, text_address, address, 0);
                self.builder.ins().store(flags, length, address, LEN_OFFSET);
                self.builder
                    .ins()
                    .store(flags, zero, address, 2 * LEN_OFFSET);
            }
            MirValue::Struct { shape, fields } => {
                let offsets: Vec<u32> = self
                    .ctx
                    .layouts
                    .layout(shape.ty)
                    .map(|layout| {
                        layout
                            .fields()
                            .iter()
                            .map(|&(_, _, offset)| offset)
                            .collect()
                    })
                    .unwrap_or_default();
                for (field, offset) in fields.iter().zip(offsets) {
                    let field_address = self.offset_address(address, offset);
                    self.store_constant(field_address, field)?;
                }
            }
            MirValue::Variant {
                shape,
                variant,
                fields,
            } => {
                let tag = self.iconst(types::I32, u64::from(*variant));
                self.builder.ins().store(flags, tag, address, 0);
                for (index, field) in fields.iter().enumerate() {
                    let index = u32::try_from(index).expect("variants have few fields");
                    let offset = self
                        .ctx
                        .layouts
                        .variant_field_offset(shape.ty, *variant, index)
                        .ok_or_else(|| CodegenError(format!("no layout for {}", shape.ty)))?;
                    let field_address = self.offset_address(address, offset);
                    self.store_constant(field_address, field)?;
                }
            }
            MirValue::List { ty, .. } | MirValue::Map { ty, .. } => {
                // Constants hold only empty collections.
                self.emitter().init_collection(address, *ty);
            }
            scalar => {
                if let Some(value) = self.constant(scalar) {
                    self.builder.ins().store(flags, value, address, 0);
                }
            }
        }
        Ok(())
    }

    fn constant(&mut self, value: &MirValue) -> Option<Value> {
        Some(match value {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "bit pattern"
            )]
            MirValue::Int { value, ty } => self.iconst(repr::int_type(*ty), *value as u64),
            #[allow(
                clippy::cast_possible_truncation,
                reason = "f32 values are stored exactly"
            )]
            MirValue::Float {
                value,
                ty: FloatTy::F32,
            } => self.builder.ins().f32const(*value as f32),
            MirValue::Float {
                value,
                ty: FloatTy::F64,
            } => self.builder.ins().f64const(*value),
            MirValue::Bool(value) => self.iconst(types::I8, u64::from(*value)),
            MirValue::Char(value) => self.iconst(types::I32, u64::from(u32::from(*value))),
            MirValue::Duration(nanos) => self.iconst_signed(types::I64, *nanos),
            MirValue::Nothing
            | MirValue::Str(_)
            | MirValue::Struct { .. }
            | MirValue::Variant { .. }
            | MirValue::Boxed(_)
            | MirValue::List { .. }
            | MirValue::Map { .. }
            | MirValue::Function(_) => return None,
        })
    }

    // ----- Statements --------------------------------------------------------------------

    fn statement(&mut self, statement: &Statement) -> Result<(), CodegenError> {
        match statement {
            Statement::Assign { place, value, span } => self.assign(place, value, *span)?,
            Statement::Drop { place, flag } => {
                let ty = self.place_ty(place);
                if !self.ctx.layouts.needs_drop(ty) {
                    return Ok(());
                }
                let continue_block = match flag {
                    Some(flag) => {
                        let set = self.read_scalar(&Place::Local(*flag), types::I8)?;
                        let drop_block = self.builder.create_block();
                        let continue_block = self.builder.create_block();
                        self.builder
                            .ins()
                            .brif(set, drop_block, &[], continue_block, &[]);
                        self.builder.switch_to_block(drop_block);
                        Some(continue_block)
                    }
                    None => None,
                };
                if let Ty::Box(inner) = ty {
                    // A box is a scalar: the pointer to its value.
                    let value = self.read_scalar(place, self.pointer)?;
                    self.emitter().drop_box(value, *inner)?;
                } else if let Ty::Fn(_) = ty {
                    // So is a function value: the pointer to its object.
                    let object = self.read_scalar(place, self.pointer)?;
                    self.emitter().drop_function_object(object);
                } else {
                    let address = self.place_address(place)?;
                    self.emitter().drop_value(address, ty)?;
                }
                if let Some(continue_block) = continue_block {
                    self.builder.ins().jump(continue_block, &[]);
                    self.builder.switch_to_block(continue_block);
                }
            }
            Statement::BindRef { local, place } => {
                let address = self.place_address(place)?;
                match self.storage.get(*local).copied() {
                    Some(Storage::Ref(var)) => self.builder.def_var(var, address),
                    _ => {
                        return Err(CodegenError(format!(
                            "binding {local:?} in `{}` is not a reference",
                            self.body.name
                        )));
                    }
                }
            }
            // Nothing remains to destroy: the value was taken apart.
            Statement::MarkMoved(_) => {}
            Statement::ListPush { list, value } => {
                let element = self.ty(value);
                let list = self.place_address(list)?;
                let slot = self.emitter().list_push_slot(list, element);
                self.store_operand(slot, value, element)?;
            }
            Statement::ListInsert { list, index, value } => {
                let element = self.ty(value);
                let list = self.place_address(list)?;
                let index = self
                    .operand(index)?
                    .ok_or_else(|| CodegenError("an index without a value".into()))?;
                let slot = self.emitter().list_open(list, index, element);
                self.store_operand(slot, value, element)?;
            }
            Statement::ListSwap {
                list,
                first,
                second,
            } => self.list_swap(list, first, second)?,
            Statement::Clear(place) => {
                let ty = self.place_ty(place);
                let collection = self.place_address(place)?;
                self.emitter().clear(collection, ty)?;
            }
            Statement::Append { target, parts } => {
                let target = self.place_address(target)?;
                for part in parts {
                    self.push_part(target, part)?;
                }
            }
            Statement::Print {
                parts,
                stream,
                newline,
            } => self.print(parts, *stream, *newline)?,
        }
        Ok(())
    }

    /// Computes `value` into `place`.
    fn assign(&mut self, place: &Place, value: &Rvalue, span: Span) -> Result<(), CodegenError> {
        let ty = self.place_ty(place);
        match self.ctx.layouts.repr(ty) {
            Repr::Memory { .. } => {
                let dest = self.place_address(place)?;
                self.assign_memory(dest, ty, value)?;
            }
            Repr::Scalar(_) => {
                if let Some(value) = self.rvalue(value, span)? {
                    self.write_scalar(place, value)?;
                }
            }
            // An intrinsic is the only computation with effects besides its result.
            Repr::None => {
                if let Rvalue::Intrinsic { .. } = value {
                    self.rvalue(value, span)?;
                }
            }
        }
        Ok(())
    }

    /// Exchanges two elements of the list in `list`, whose indexes are checked.
    fn list_swap(
        &mut self,
        list: &Place,
        first: &Operand,
        second: &Operand,
    ) -> Result<(), CodegenError> {
        let Ty::List(element) = self.place_ty(list) else {
            return Err(CodegenError("swap of a value that is not a list".into()));
        };
        let (size, align) = self.ctx.layouts.size_align(*element);
        if size == 0 {
            return Ok(());
        }
        let list = self.place_address(list)?;
        let mut indexes = Vec::new();
        for index in [first, second] {
            indexes.push(
                self.operand(index)?
                    .ok_or_else(|| CodegenError("an index without a value".into()))?,
            );
        }
        // The copies must not overlap: swapping an element with itself does nothing.
        let (swap_block, done) = (self.builder.create_block(), self.builder.create_block());
        let same = self
            .builder
            .ins()
            .icmp(IntCC::Equal, indexes[0], indexes[1]);
        self.builder.ins().brif(same, done, &[], swap_block, &[]);
        self.builder.switch_to_block(swap_block);
        let a = self.emitter().element_addr(list, indexes[0], *element);
        let b = self.emitter().element_addr(list, indexes[1], *element);
        let slot = self.stack_slot(size, align);
        let temp = self.slot_address(slot);
        self.copy_memory(temp, a, size, align);
        self.copy_memory(a, b, size, align);
        self.copy_memory(b, temp, size, align);
        self.builder.ins().jump(done, &[]);
        self.builder.switch_to_block(done);
        Ok(())
    }

    /// Computes a value of type `ty`, held in memory, into `dest`.
    fn assign_memory(&mut self, dest: Value, ty: Ty, value: &Rvalue) -> Result<(), CodegenError> {
        match value {
            Rvalue::Use(operand) => {
                let source = self.operand_address(operand)?;
                self.copy_value(dest, source, ty);
            }
            Rvalue::Struct { fields, .. } => {
                let layout_fields: Vec<(Ty, u32)> = self
                    .ctx
                    .layouts
                    .layout(ty)
                    .map(|layout| {
                        layout
                            .fields()
                            .iter()
                            .map(|&(_, ty, offset)| (ty, offset))
                            .collect()
                    })
                    .unwrap_or_default();
                for (operand, (field_ty, offset)) in fields.iter().zip(layout_fields) {
                    let field_address = self.offset_address(dest, offset);
                    self.store_operand(field_address, operand, field_ty)?;
                }
            }
            Rvalue::Variant {
                variant, fields, ..
            } => {
                let tag = self.iconst(types::I32, u64::from(*variant));
                self.builder
                    .ins()
                    .store(MemFlagsData::trusted(), tag, dest, 0);
                for (index, operand) in fields.iter().enumerate() {
                    let index = u32::try_from(index).expect("variants have few fields");
                    let offset = self
                        .ctx
                        .layouts
                        .variant_field_offset(ty, *variant, index)
                        .ok_or_else(|| CodegenError(format!("no layout for {ty}")))?;
                    let field_ty = self.ty(operand);
                    let field_address = self.offset_address(dest, offset);
                    self.store_operand(field_address, operand, field_ty)?;
                }
            }
            Rvalue::Unbox(operand) => {
                let value = self
                    .operand(operand)?
                    .ok_or_else(|| CodegenError("a box without a value".into()))?;
                self.copy_value(dest, value, ty);
                self.emitter().free(value, ty);
            }
            Rvalue::Interpolate(parts) => {
                self.call_runtime(self.ctx.runtime.string_new, &[dest]);
                for part in parts {
                    self.push_part(dest, part)?;
                }
            }
            Rvalue::Binary {
                op: BinaryOp::Concat,
                lhs,
                rhs,
            } if ty != Ty::String => {
                let (lhs, rhs) = (self.operand_address(lhs)?, self.operand_address(rhs)?);
                self.emitter().list_concat(dest, ty, &[lhs, rhs])?;
            }
            Rvalue::Binary {
                op: BinaryOp::Concat,
                lhs,
                rhs,
            } => {
                let (lhs, rhs) = (self.operand_address(lhs)?, self.operand_address(rhs)?);
                self.call_runtime(self.ctx.runtime.string_new, &[dest]);
                self.call_runtime(self.ctx.runtime.string_push_string, &[dest, lhs]);
                self.call_runtime(self.ctx.runtime.string_push_string, &[dest, rhs]);
            }
            Rvalue::Clone(place) => {
                let source = self.place_address(place)?;
                self.emitter().clone_value(dest, source, ty)?;
            }
            Rvalue::Intrinsic { intrinsic, args } => self.call_intrinsic(*intrinsic, args, dest)?,
            Rvalue::List { .. }
            | Rvalue::Map { .. }
            | Rvalue::MapInsert { .. }
            | Rvalue::MapRemove { .. }
            | Rvalue::ListPop(_)
            | Rvalue::ListRemove { .. }
            | Rvalue::MapKeys(_)
            | Rvalue::MapValues(_) => self.assign_collection(dest, ty, value)?,
            other => {
                return Err(CodegenError(format!(
                    "cannot compute {other:?} into memory"
                )));
            }
        }
        Ok(())
    }

    /// Computes a collection, or an option or element taken out of one, into `dest`.
    fn assign_collection(
        &mut self,
        dest: Value,
        ty: Ty,
        value: &Rvalue,
    ) -> Result<(), CodegenError> {
        match value {
            Rvalue::List { elements, .. } => {
                let Ty::List(element) = ty else {
                    return Err(CodegenError(format!("a list literal of type {ty}")));
                };
                self.emitter().init_collection(dest, ty);
                let count = self.iconst(types::I64, elements.len() as u64);
                self.emitter().list_reserve(dest, count, *element);
                for operand in elements {
                    let slot = self.emitter().list_push_slot(dest, *element);
                    self.store_operand(slot, operand, *element)?;
                }
            }
            Rvalue::Map { entries, .. } => {
                self.emitter().init_collection(dest, ty);
                let entry = self.ctx.layouts.entry(ty);
                for (key, value) in entries {
                    let key = self.operand_in_memory(key, entry.key)?;
                    let value = self.operand_in_memory(value, entry.value)?;
                    self.emitter().map_insert(dest, ty, key, value, None)?;
                }
            }
            Rvalue::MapInsert { map, key, value } => {
                let map_ty = self.place_ty(map);
                let entry = self.ctx.layouts.entry(map_ty);
                let map = self.place_address(map)?;
                let key = self.operand_in_memory(key, entry.key)?;
                let value = self.operand_in_memory(value, entry.value)?;
                self.emitter()
                    .map_insert(map, map_ty, key, value, Some((dest, ty)))?;
            }
            Rvalue::MapRemove { map, key } => {
                let map_ty = self.place_ty(map);
                let entry = self.ctx.layouts.entry(map_ty);
                let map = self.place_address(map)?;
                let key = self.operand_in_memory(key, entry.key)?;
                self.emitter().map_remove(map, map_ty, key, dest, ty)?;
            }
            Rvalue::ListPop(list) => {
                let element = element_of(self.place_ty(list));
                let list = self.place_address(list)?;
                self.emitter().list_pop(list, element, dest, ty)?;
            }
            Rvalue::ListRemove { list, index } => {
                let list = self.place_address(list)?;
                let index = self
                    .operand(index)?
                    .ok_or_else(|| CodegenError("an index without a value".into()))?;
                let mut emitter = self.emitter();
                let element = emitter.element_addr(list, index, ty);
                emitter.copy_bytes(dest, element, ty);
                emitter.list_close(list, index, ty);
            }
            Rvalue::MapKeys(map) | Rvalue::MapValues(map) => {
                let map_ty = self.place_ty(map);
                let map = self.place_address(map)?;
                let keys = matches!(value, Rvalue::MapKeys(_));
                self.emitter().map_parts(dest, map, map_ty, keys)?;
            }
            other => {
                return Err(CodegenError(format!(
                    "{other:?} is not a collection operation"
                )));
            }
        }
        Ok(())
    }

    /// A scalar computed from a collection: its length, a position, a removed element or a
    /// membership test.
    fn collection_scalar(&mut self, rvalue: &Rvalue) -> Result<Option<Value>, CodegenError> {
        Ok(match rvalue {
            Rvalue::Len(place) => {
                let collection = self.place_address(place)?;
                Some(self.emitter().len(collection))
            }
            Rvalue::MapFind { map, key } => {
                let map_ty = self.place_ty(map);
                let entry = self.ctx.layouts.entry(map_ty);
                let map = self.place_address(map)?;
                let key = self.operand_in_memory(key, entry.key)?;
                Some(self.emitter().map_find(map, map_ty, key)?.0)
            }
            Rvalue::ListRemove { list, index } => {
                let element = element_of(self.place_ty(list));
                let list = self.place_address(list)?;
                let index = self
                    .operand(index)?
                    .ok_or_else(|| CodegenError("an index without a value".into()))?;
                let mut emitter = self.emitter();
                let address = emitter.element_addr(list, index, element);
                let value = match emitter.load_value(address, element) {
                    Loaded::Scalar(value) => Some(value),
                    _ => None,
                };
                emitter.list_close(list, index, element);
                value
            }
            Rvalue::Contains { collection, value } => {
                let ty = self.place_ty(collection);
                let collection = self.place_address(collection)?;
                if let Ty::List(element) = ty {
                    let value = self.load_operand(value)?;
                    Some(self.emitter().list_contains(collection, *element, value)?)
                } else {
                    {
                        let key_ty = self.ctx.layouts.entry(ty).key;
                        let key = self.operand_in_memory(value, key_ty)?;
                        let (position, _) = self.emitter().map_find(collection, ty, key)?;
                        let zero = self.iconst(types::I64, 0);
                        Some(self.builder.ins().icmp(
                            IntCC::SignedGreaterThanOrEqual,
                            position,
                            zero,
                        ))
                    }
                }
            }
            other => {
                return Err(CodegenError(format!(
                    "{other:?} is not a collection operation"
                )));
            }
        })
    }

    /// A new box (its pointer), or a scalar taken out of a box.
    fn box_scalar(&mut self, rvalue: &Rvalue) -> Result<Option<Value>, CodegenError> {
        Ok(match rvalue {
            Rvalue::BoxNew(operand) => {
                let ty = self.ty(operand);
                let pointer = self.emitter().alloc(ty);
                self.store_operand(pointer, operand, ty)?;
                Some(pointer)
            }
            Rvalue::Unbox(operand) => {
                let Ty::Box(inner) = self.ty(operand) else {
                    return Err(CodegenError("unbox of a value that is not a box".into()));
                };
                let pointer = self
                    .operand(operand)?
                    .ok_or_else(|| CodegenError("a box without a value".into()))?;
                let value = match self.ctx.layouts.repr(*inner) {
                    Repr::Scalar(scalar) => Some(self.builder.ins().load(
                        scalar,
                        MemFlagsData::trusted(),
                        pointer,
                        0,
                    )),
                    _ => None,
                };
                self.emitter().free(pointer, *inner);
                value
            }
            other => return Err(CodegenError(format!("{other:?} is not a box operation"))),
        })
    }

    fn rvalue(&mut self, rvalue: &Rvalue, span: Span) -> Result<Option<Value>, CodegenError> {
        Ok(match rvalue {
            Rvalue::Use(operand) => self.operand(operand)?,
            Rvalue::Closure { func, captures, .. } => Some(self.function_value(*func, captures)?),
            Rvalue::Unary { op, operand } => {
                let ty = self.ty(operand);
                let Some(value) = self.operand(operand)? else {
                    return Ok(None);
                };
                Some(self.unary(*op, value, ty, span))
            }
            Rvalue::Binary { op, lhs, rhs } => {
                let (lhs_ty, rhs_ty) = (self.ty(lhs), self.ty(rhs));
                if lhs_ty == Ty::String {
                    return self.string_comparison(*op, lhs, rhs).map(Some);
                }
                if let Ty::Adt(_)
                | Ty::Option(_)
                | Ty::Box(_)
                | Ty::List(_)
                | Ty::Map(_)
                | Ty::Set(_) = lhs_ty
                {
                    let (a, b) = (self.load_operand(lhs)?, self.load_operand(rhs)?);
                    let ordering = self.emitter().compare(lhs_ty, a, b)?;
                    return Ok(Some(self.ordering_test(*op, ordering)));
                }
                let (a, b) = (self.operand(lhs)?, self.operand(rhs)?);
                match (a, b) {
                    (Some(a), Some(b)) => Some(self.binary(*op, a, b, lhs_ty, rhs_ty, span)?),
                    // Only `nothing` has no representation; all its values are equal.
                    _ => Some(self.iconst(types::I8, u64::from(*op == BinaryOp::Eq))),
                }
            }
            Rvalue::Cast { kind, operand, to } => {
                let from = self.ty(operand);
                let Some(value) = self.operand(operand)? else {
                    return Ok(None);
                };
                Some(self.cast(*kind, value, from, *to, span))
            }
            Rvalue::Intrinsic { intrinsic, args } => self.intrinsic_scalar(*intrinsic, args)?,
            Rvalue::StringLen(operand) => {
                let address = self.operand_address(operand)?;
                let length = self.builder.ins().load(
                    self.pointer,
                    MemFlagsData::trusted(),
                    address,
                    LEN_OFFSET,
                );
                Some(length)
            }
            Rvalue::Clone(place) => {
                let ty = self.place_ty(place);
                let Repr::Scalar(scalar) = self.ctx.layouts.repr(ty) else {
                    return Ok(None);
                };
                let value = self.read_scalar(place, scalar)?;
                match ty {
                    Ty::Box(inner) => Some(self.emitter().clone_box(value, *inner)?),
                    Ty::Fn(_) => Some(self.emitter().clone_function_object(value)),
                    _ => Some(value),
                }
            }
            Rvalue::Discriminant(place) => {
                let address = self.place_address(place)?;
                Some(
                    self.builder
                        .ins()
                        .load(types::I32, MemFlagsData::trusted(), address, 0),
                )
            }
            Rvalue::BoxNew(_) | Rvalue::Unbox(_) => self.box_scalar(rvalue)?,
            Rvalue::Len(_)
            | Rvalue::MapFind { .. }
            | Rvalue::ListRemove { .. }
            | Rvalue::Contains { .. } => self.collection_scalar(rvalue)?,
            Rvalue::Interpolate(_)
            | Rvalue::Struct { .. }
            | Rvalue::Variant { .. }
            | Rvalue::List { .. }
            | Rvalue::Map { .. }
            | Rvalue::ListPop(_)
            | Rvalue::MapInsert { .. }
            | Rvalue::MapRemove { .. }
            | Rvalue::MapKeys(_)
            | Rvalue::MapValues(_) => {
                return Err(CodegenError(format!(
                    "{rvalue:?} does not produce a scalar"
                )));
            }
        })
    }

    /// A comparison or substring test of two strings.
    /// The result of `intrinsic` called with `args`, if it is a scalar; `None` for `nothing`.
    fn intrinsic_scalar(
        &mut self,
        intrinsic: Intrinsic,
        args: &[Operand],
    ) -> Result<Option<Value>, CodegenError> {
        let Repr::Scalar(scalar) = self.ctx.layouts.repr(kind_ty(intrinsic.ret())) else {
            // The result of `nothing` has nowhere to go.
            let slot = self.stack_slot(1, 1);
            let out = self.slot_address(slot);
            self.call_intrinsic(intrinsic, args, out)?;
            return Ok(None);
        };
        let slot = self.stack_slot(scalar.bytes(), u8::try_from(scalar.bytes()).unwrap_or(8));
        let out = self.slot_address(slot);
        self.call_intrinsic(intrinsic, args, out)?;
        Ok(Some(self.builder.ins().load(
            scalar,
            MemFlagsData::trusted(),
            out,
            0,
        )))
    }

    /// Calls `intrinsic` with `args`, writing its result to `out`: each argument is passed by
    /// the address of its value, in an array.
    fn call_intrinsic(
        &mut self,
        intrinsic: Intrinsic,
        args: &[Operand],
        out: Value,
    ) -> Result<(), CodegenError> {
        let pointer_size = self.pointer.bytes();
        let count = u32::try_from(args.len().max(1)).expect("intrinsics have few parameters");
        let array_slot = self.stack_slot(
            count * pointer_size,
            u8::try_from(pointer_size).unwrap_or(8),
        );
        let array = self.slot_address(array_slot);
        for (index, arg) in args.iter().enumerate() {
            let ty = self.ty(arg);
            let address = self.operand_in_memory(arg, ty)?;
            let offset = i32::try_from(index).expect("few parameters")
                * i32::try_from(pointer_size).expect("small pointers");
            self.builder
                .ins()
                .store(MemFlagsData::trusted(), address, array, offset);
        }
        let code = self.iconst(types::I32, u64::from(intrinsic as u32));
        self.call_runtime(self.ctx.runtime.intrinsic, &[code, array, out]);
        Ok(())
    }

    fn string_comparison(
        &mut self,
        op: BinaryOp,
        lhs: &Operand,
        rhs: &Operand,
    ) -> Result<Value, CodegenError> {
        let (a, b) = (self.operand_address(lhs)?, self.operand_address(rhs)?);
        if op == BinaryOp::Contains {
            // `needle in haystack`
            let contains = self.call_runtime(self.ctx.runtime.string_contains, &[b, a]);
            return Ok(contains.expect("returns a value"));
        }
        let ordering = self
            .call_runtime(self.ctx.runtime.string_compare, &[a, b])
            .expect("returns a value");
        let zero = self.iconst(types::I32, 0);
        Ok(self
            .builder
            .ins()
            .icmp(int_condition(op, true), ordering, zero))
    }

    fn unary(&mut self, op: UnaryOp, value: Value, ty: Ty, span: Span) -> Value {
        match (op, ty) {
            (UnaryOp::Neg, Ty::Float(_)) => self.builder.ins().fneg(value),
            (UnaryOp::Neg, Ty::Int(_) | Ty::Duration) => {
                let int = match ty {
                    Ty::Int(int) => int,
                    _ => IntTy::I64,
                };
                let cl = repr::int_type(int);
                let min = self.iconst(cl, 1u64 << (int.bits() - 1));
                let is_min = self.builder.ins().icmp(IntCC::Equal, value, min);
                self.panic_if(is_min, PanicKind::Overflow, span);
                self.builder.ins().ineg(value)
            }
            (UnaryOp::Not, _) => {
                let one = self.iconst(types::I8, 1);
                self.builder.ins().bxor(value, one)
            }
            (UnaryOp::BitNot, _) => self.builder.ins().bnot(value),
            (UnaryOp::Neg, _) => unreachable!("negation of {ty}"),
        }
    }

    fn binary(
        &mut self,
        op: BinaryOp,
        a: Value,
        b: Value,
        lhs_ty: Ty,
        rhs_ty: Ty,
        span: Span,
    ) -> Result<Value, CodegenError> {
        Ok(match lhs_ty {
            Ty::Int(int) => {
                let rhs_int = match rhs_ty {
                    Ty::Int(rhs_int) => rhs_int,
                    _ => int,
                };
                self.int_binary(op, a, b, int, rhs_int, span)
            }
            Ty::Duration => self.int_binary(op, a, b, IntTy::I64, IntTy::I64, span),
            Ty::Float(float) => self.float_binary(op, a, b, float),
            Ty::Bool | Ty::Char => {
                let cc = int_condition(op, false);
                self.builder.ins().icmp(cc, a, b)
            }
            other => return Err(CodegenError(format!("binary operation {op:?} on {other}"))),
        })
    }

    fn int_binary(
        &mut self,
        op: BinaryOp,
        a: Value,
        b: Value,
        int: IntTy,
        rhs_int: IntTy,
        span: Span,
    ) -> Value {
        let signed = int.is_signed();
        let cl = repr::int_type(int);
        match op {
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul => {
                let (result, overflow) = match (op, signed) {
                    (BinaryOp::Add, true) => self.builder.ins().sadd_overflow(a, b),
                    (BinaryOp::Add, false) => self.builder.ins().uadd_overflow(a, b),
                    (BinaryOp::Sub, true) => self.builder.ins().ssub_overflow(a, b),
                    (BinaryOp::Sub, false) => self.builder.ins().usub_overflow(a, b),
                    (_, true) => self.builder.ins().smul_overflow(a, b),
                    (_, false) => self.builder.ins().umul_overflow(a, b),
                };
                self.panic_if(overflow, PanicKind::Overflow, span);
                result
            }
            BinaryOp::Div | BinaryOp::Rem => {
                let zero = self.iconst(cl, 0);
                let is_zero = self.builder.ins().icmp(IntCC::Equal, b, zero);
                self.panic_if(is_zero, PanicKind::DivisionByZero, span);
                if !signed {
                    return if op == BinaryOp::Div {
                        self.builder.ins().udiv(a, b)
                    } else {
                        self.builder.ins().urem(a, b)
                    };
                }
                let minus_one = self.iconst_signed(cl, -1);
                let divisor_is_minus_one = self.builder.ins().icmp(IntCC::Equal, b, minus_one);
                if op == BinaryOp::Div {
                    let min = self.iconst(cl, 1u64 << (int.bits() - 1));
                    let dividend_is_min = self.builder.ins().icmp(IntCC::Equal, a, min);
                    let overflow = self
                        .builder
                        .ins()
                        .band(dividend_is_min, divisor_is_minus_one);
                    self.panic_if(overflow, PanicKind::Overflow, span);
                    self.builder.ins().sdiv(a, b)
                } else {
                    // `x % -1` is 0; dividing by 1 instead avoids the hardware trap for MIN.
                    let one = self.iconst(cl, 1);
                    let divisor = self.builder.ins().select(divisor_is_minus_one, one, b);
                    self.builder.ins().srem(a, divisor)
                }
            }
            BinaryOp::WrappingAdd => self.builder.ins().iadd(a, b),
            BinaryOp::WrappingSub => self.builder.ins().isub(a, b),
            BinaryOp::BitAnd => self.builder.ins().band(a, b),
            BinaryOp::BitOr => self.builder.ins().bor(a, b),
            BinaryOp::BitXor => self.builder.ins().bxor(a, b),
            BinaryOp::Shl | BinaryOp::Shr => {
                let rhs_cl = repr::int_type(rhs_int);
                let bits = self.iconst(rhs_cl, u64::from(int.bits()));
                let too_large = if rhs_int.is_signed() {
                    self.builder
                        .ins()
                        .icmp(IntCC::SignedGreaterThanOrEqual, b, bits)
                } else {
                    self.builder
                        .ins()
                        .icmp(IntCC::UnsignedGreaterThanOrEqual, b, bits)
                };
                let invalid = if rhs_int.is_signed() {
                    let zero = self.iconst(rhs_cl, 0);
                    let negative = self.builder.ins().icmp(IntCC::SignedLessThan, b, zero);
                    self.builder.ins().bor(too_large, negative)
                } else {
                    too_large
                };
                self.panic_if(invalid, PanicKind::ShiftOverflow, span);
                let amount = match rhs_int.bits().cmp(&int.bits()) {
                    std::cmp::Ordering::Greater => self.builder.ins().ireduce(cl, b),
                    std::cmp::Ordering::Less => self.builder.ins().uextend(cl, b),
                    std::cmp::Ordering::Equal => b,
                };
                match (op, signed) {
                    (BinaryOp::Shl, _) => self.builder.ins().ishl(a, amount),
                    (_, true) => self.builder.ins().sshr(a, amount),
                    (_, false) => self.builder.ins().ushr(a, amount),
                }
            }
            BinaryOp::Eq
            | BinaryOp::Ne
            | BinaryOp::Lt
            | BinaryOp::Le
            | BinaryOp::Gt
            | BinaryOp::Ge => self.builder.ins().icmp(int_condition(op, signed), a, b),
            BinaryOp::Concat | BinaryOp::Contains => unreachable!("string operations at runtime"),
        }
    }

    fn float_binary(&mut self, op: BinaryOp, a: Value, b: Value, float: FloatTy) -> Value {
        match op {
            BinaryOp::Add => self.builder.ins().fadd(a, b),
            BinaryOp::Sub => self.builder.ins().fsub(a, b),
            BinaryOp::Mul => self.builder.ins().fmul(a, b),
            BinaryOp::Div => self.builder.ins().fdiv(a, b),
            BinaryOp::Rem => {
                let function = match float {
                    FloatTy::F64 => self.ctx.runtime.rem_f64,
                    FloatTy::F32 => self.ctx.runtime.rem_f32,
                };
                self.call_runtime(function, &[a, b])
                    .expect("remainder returns a value")
            }
            _ => {
                let cc = match op {
                    BinaryOp::Eq => FloatCC::Equal,
                    BinaryOp::Ne => FloatCC::NotEqual,
                    BinaryOp::Lt => FloatCC::LessThan,
                    BinaryOp::Le => FloatCC::LessThanOrEqual,
                    BinaryOp::Gt => FloatCC::GreaterThan,
                    BinaryOp::Ge => FloatCC::GreaterThanOrEqual,
                    _ => unreachable!("not a float operation: {op:?}"),
                };
                self.builder.ins().fcmp(cc, a, b)
            }
        }
    }

    fn cast(&mut self, kind: CastKind, value: Value, from: Ty, to: Ty, span: Span) -> Value {
        match (from, to) {
            _ if from == to => value,
            (Ty::Int(_), Ty::Int(_)) if kind == CastKind::Reinterpret => value,
            (Ty::Int(source), Ty::Int(target)) => self.int_cast(value, source, target, span),
            (Ty::Int(source), Ty::Float(target)) => {
                let float = repr::scalar(Ty::Float(target)).expect("floats are scalars");
                let wide = self.resize(value, source, IntTy::I64);
                if source.is_signed() {
                    self.builder.ins().fcvt_from_sint(float, wide)
                } else {
                    self.builder.ins().fcvt_from_uint(float, wide)
                }
            }
            (Ty::Float(_), Ty::Int(target)) => self.float_to_int(value, target),
            (Ty::Float(FloatTy::F32), Ty::Float(FloatTy::F64)) => {
                self.builder.ins().fpromote(types::F64, value)
            }
            (Ty::Float(FloatTy::F64), Ty::Float(FloatTy::F32)) => {
                self.builder.ins().fdemote(types::F32, value)
            }
            // `char` to `u32` keeps the scalar value.
            _ => value,
        }
    }

    /// Converts between integer widths: extends according to the source's signedness, or
    /// keeps the low bits.
    fn resize(&mut self, value: Value, from: IntTy, to: IntTy) -> Value {
        let to_cl = repr::int_type(to);
        match to.bits().cmp(&from.bits()) {
            std::cmp::Ordering::Greater if from.is_signed() => {
                self.builder.ins().sextend(to_cl, value)
            }
            std::cmp::Ordering::Greater => self.builder.ins().uextend(to_cl, value),
            std::cmp::Ordering::Less => self.builder.ins().ireduce(to_cl, value),
            std::cmp::Ordering::Equal => value,
        }
    }

    /// An integer conversion with `as`, which panics if the value does not fit the target.
    fn int_cast(&mut self, value: Value, source: IntTy, target: IntTy, span: Span) -> Value {
        let converted = self.resize(value, source, target);
        let back = self.resize(converted, target, source);
        let mut lossy = self.builder.ins().icmp(IntCC::NotEqual, back, value);
        if source.is_signed() != target.is_signed() {
            // A sign change is lossy if the signed side is negative.
            let (signed_value, signed_ty) = if source.is_signed() {
                (value, source)
            } else {
                (converted, target)
            };
            let zero = self.iconst(repr::int_type(signed_ty), 0);
            let negative = self
                .builder
                .ins()
                .icmp(IntCC::SignedLessThan, signed_value, zero);
            lossy = self.builder.ins().bor(lossy, negative);
        }
        self.panic_if(lossy, PanicKind::LossyCast, span);
        converted
    }

    /// A float-to-integer conversion: saturating, with NaN converting to 0.
    fn float_to_int(&mut self, value: Value, target: IntTy) -> Value {
        let cl = repr::int_type(target);
        if target.is_signed() {
            let wide = self.builder.ins().fcvt_to_sint_sat(types::I64, value);
            if target.bits() == 64 {
                return wide;
            }
            let (min, max) = (
                1i64 << (target.bits() - 1),
                (1i64 << (target.bits() - 1)) - 1,
            );
            let min = self.iconst_signed(types::I64, -min);
            let max = self.iconst_signed(types::I64, max);
            let clamped = self.builder.ins().smax(wide, min);
            let clamped = self.builder.ins().smin(clamped, max);
            self.builder.ins().ireduce(cl, clamped)
        } else {
            let wide = self.builder.ins().fcvt_to_uint_sat(types::I64, value);
            if target.bits() == 64 {
                return wide;
            }
            let max = self.iconst(types::I64, (1u64 << target.bits()) - 1);
            let clamped = self.builder.ins().umin(wide, max);
            self.builder.ins().ireduce(cl, clamped)
        }
    }

    // ----- Comparison --------------------------------------------------------------------

    /// The result of comparison `op` from an ordering code.
    fn ordering_test(&mut self, op: BinaryOp, ordering: Value) -> Value {
        let is = |this: &mut Self, code: u64| {
            let code = this.iconst(types::I8, code);
            this.builder.ins().icmp(IntCC::Equal, ordering, code)
        };
        match op {
            BinaryOp::Eq => is(self, EQUAL),
            BinaryOp::Ne => {
                let equal = is(self, EQUAL);
                let one = self.iconst(types::I8, 1);
                self.builder.ins().bxor(equal, one)
            }
            BinaryOp::Lt => is(self, LESS),
            BinaryOp::Gt => is(self, GREATER),
            BinaryOp::Le | BinaryOp::Ge => {
                let equal = is(self, EQUAL);
                let strict = is(self, if op == BinaryOp::Le { LESS } else { GREATER });
                self.builder.ins().bor(equal, strict)
            }
            _ => unreachable!("not a comparison: {op:?}"),
        }
    }

    // ----- Output ------------------------------------------------------------------------

    fn print(
        &mut self,
        parts: &[PrintPart],
        stream: Stream,
        newline: bool,
    ) -> Result<(), CodegenError> {
        let stream = self.iconst(types::I32, if stream == Stream::Stderr { 2 } else { 1 });
        for part in parts {
            match part {
                PrintPart::Text(text) if text.is_empty() => {}
                PrintPart::Text(text) => {
                    let (address, length) = self.text_address(text)?;
                    self.call_runtime(self.ctx.runtime.print_str, &[address, length, stream]);
                }
                PrintPart::Value(operand) => self.print_value(operand, stream)?,
            }
        }
        if newline {
            self.call_runtime(self.ctx.runtime.print_newline, &[stream]);
        }
        Ok(())
    }

    /// Prints a value as `:put` displays it.
    fn print_value(&mut self, operand: &Operand, stream: Value) -> Result<(), CodegenError> {
        let ty = self.ty(operand);
        let value = self.load_operand(operand)?;
        let runtime = &self.ctx.runtime;
        let (function, value) = match (ty, value) {
            (Ty::String, Loaded::Address(address)) => (runtime.print_string, address),
            (Ty::Int(int), Loaded::Scalar(value)) if int.is_signed() => {
                (runtime.print_i64, self.resize(value, int, IntTy::I64))
            }
            (Ty::Int(int), Loaded::Scalar(value)) => {
                (runtime.print_u64, self.resize(value, int, IntTy::U64))
            }
            (Ty::Float(FloatTy::F64), Loaded::Scalar(value)) => (runtime.print_f64, value),
            (Ty::Float(FloatTy::F32), Loaded::Scalar(value)) => (runtime.print_f32, value),
            (Ty::Bool, Loaded::Scalar(value)) => (runtime.print_bool, value),
            (Ty::Char, Loaded::Scalar(value)) => (runtime.print_char, value),
            (Ty::Duration, Loaded::Scalar(value)) => (runtime.print_duration, value),
            (Ty::Nothing, _) => return Ok(()),
            // Values with parts are formatted into a temporary string first.
            (_, value) => {
                let slot = self.stack_slot(STRING_SIZE, 8);
                let text = self.slot_address(slot);
                self.call_runtime(self.ctx.runtime.string_new, &[text]);
                self.emitter().display(ty, value, text, false)?;
                self.call_runtime(self.ctx.runtime.print_string, &[text, stream]);
                self.call_runtime(self.ctx.runtime.string_drop, &[text]);
                return Ok(());
            }
        };
        self.call_runtime(function, &[value, stream]);
        Ok(())
    }

    /// Appends a piece of an interpolated string to the string at `dest`.
    fn push_part(&mut self, dest: Value, part: &PrintPart) -> Result<(), CodegenError> {
        match part {
            PrintPart::Text(text) => self.emitter().push_text(dest, text),
            PrintPart::Value(operand) => {
                let ty = self.ty(operand);
                let value = self.load_operand(operand)?;
                self.emitter().display(ty, value, dest, false)
            }
        }
    }

    // ----- Terminators -------------------------------------------------------------------

    fn terminator(&mut self, terminator: &Terminator) -> Result<(), CodegenError> {
        match terminator {
            Terminator::Goto(target) => {
                self.builder.ins().jump(self.blocks[*target], &[]);
            }
            Terminator::If {
                cond,
                then_block,
                else_block,
            } => {
                let cond = self
                    .operand(cond)?
                    .ok_or_else(|| CodegenError("a condition without a value".into()))?;
                self.builder.ins().brif(
                    cond,
                    self.blocks[*then_block],
                    &[],
                    self.blocks[*else_block],
                    &[],
                );
            }
            Terminator::Call {
                func,
                args,
                destination,
                target,
                on_error,
            } => self.call(func, args, destination, *target, on_error.as_ref())?,
            Terminator::Raise(error) => self.raise(error)?,
            Terminator::Return => {
                let return_local = self.body.return_local;
                match self.ctx.layouts.repr(self.body.locals[return_local].ty) {
                    Repr::Memory { size, align } => {
                        let dest = self
                            .builder
                            .use_var(self.result_pointer.expect("set for results in memory"));
                        let source = self.place_address(&Place::Local(return_local))?;
                        self.copy_memory(dest, source, size, align);
                        self.builder.ins().return_(&[]);
                    }
                    Repr::Scalar(ty) => {
                        let value = self.read_scalar(&Place::Local(return_local), ty)?;
                        self.builder.ins().return_(&[value]);
                    }
                    Repr::None => {
                        self.builder.ins().return_(&[]);
                    }
                }
            }
            Terminator::Panic {
                kind,
                message,
                span,
            } => {
                if message.is_empty() {
                    self.panic(*kind, *span);
                } else {
                    let kind = self.iconst(types::I32, *kind as u64);
                    self.call_runtime(self.ctx.runtime.panic_begin, &[kind]);
                    self.print(message, Stream::Stderr, false)?;
                    let location = self.location(*span);
                    self.call_runtime(self.ctx.runtime.panic_end, &location);
                    self.builder.ins().trap(TrapCode::unwrap_user(1));
                }
            }
            Terminator::Unreachable => {
                self.builder.ins().trap(TrapCode::unwrap_user(2));
            }
        }
        Ok(())
    }

    fn call(
        &mut self,
        func: &CallTarget,
        args: &[CallArg],
        destination: &Place,
        target: Option<BlockId>,
        on_error: Option<&ErrorTarget>,
    ) -> Result<(), CodegenError> {
        // A function value's object comes first; then where a result in memory goes.
        let (object, in_memory, raises) = match func {
            CallTarget::Direct(id) => {
                let callee = &self.program.functions[*id];
                (
                    None,
                    self.ctx.layouts.returns_in_memory(callee),
                    callee.raises,
                )
            }
            CallTarget::Value(place) => {
                let Ty::Fn(function) = self.place_ty(place) else {
                    return Err(CodegenError(
                        "a call of a value that is not a function".into(),
                    ));
                };
                let object = self.read_scalar(place, self.pointer)?;
                let in_memory = matches!(self.ctx.layouts.repr(function.ret), Repr::Memory { .. });
                (Some((object, function)), in_memory, function.raises)
            }
        };
        let mut values = Vec::with_capacity(args.len() + 3);
        if let Some((object, _)) = object {
            values.push(object);
        }
        if in_memory {
            values.push(self.place_address(destination)?);
        }
        for arg in args {
            match arg {
                CallArg::Ref { place, .. } => values.push(self.place_address(place)?),
                CallArg::Value(operand) => match self.ctx.layouts.repr(self.ty(operand)) {
                    Repr::Scalar(_) => values.extend(self.operand(operand)?),
                    // An owned value in memory is passed by address; the callee copies it.
                    Repr::Memory { .. } => values.push(self.operand_address(operand)?),
                    Repr::None => {}
                },
            }
        }
        let error_slot = if raises {
            let (size, align) = self.ctx.layouts.size_align(self.error_option());
            let slot = self.stack_slot(size, align);
            let address = self.slot_address(slot);
            self.emitter().store_none(address);
            values.push(address);
            Some(address)
        } else {
            None
        };
        let call = match (func, object) {
            (CallTarget::Value(_), Some((object, function))) => {
                let flags = MemFlagsData::trusted();
                let code =
                    self.builder
                        .ins()
                        .load(self.pointer, flags, object, crate::functions::CODE);
                let signature = self.ctx.layouts.value_signature(self.module, function);
                let signature = self.builder.import_signature(signature);
                self.builder.ins().call_indirect(signature, code, &values)
            }
            (CallTarget::Direct(id), _) => {
                let (func_id, _) = self.ctx.functions[*id].clone();
                let func_ref = self.func_ref(func_id);
                self.builder.ins().call(func_ref, &values)
            }
            (CallTarget::Value(_), None) => unreachable!("set above"),
        };
        let result = self.builder.inst_results(call).first().copied();
        if let (Some(slot), Some(on_error)) = (error_slot, on_error) {
            self.branch_on_error(slot, on_error)?;
        }
        if let Some(result) = result {
            self.write_scalar(destination, result)?;
        }
        match target {
            Some(target) => {
                self.builder.ins().jump(self.blocks[target], &[]);
            }
            None => {
                self.builder.ins().trap(TrapCode::unwrap_user(2));
            }
        }
        Ok(())
    }
}

impl Translator<'_, '_> {
    /// A new function value: the object of body `func`, holding the captured values.
    fn function_value(
        &mut self,
        func: pika_mir::InstanceId,
        captures: &[Operand],
    ) -> Result<Value, CodegenError> {
        let types = self.program.functions[func].env.clone().unwrap_or_default();
        let (offsets, size) = self.ctx.layouts.env_layout(&types);
        let (code, _) = self.ctx.functions[func].clone();
        let drop = self.ctx.env_drop(self.module, func, &types)?;
        let object = self.emitter().new_function_value(code, drop, size);
        for ((operand, &ty), offset) in captures.iter().zip(&types).zip(offsets) {
            let address = self.offset_address(object, offset);
            self.store_operand(address, operand, ty)?;
        }
        Ok(object)
    }

    /// The type of the slot where a function that raises puts its error.
    fn error_option(&self) -> Ty {
        Ty::option(self.program.types.error_ty())
    }

    /// After a call of a function that raises: continues at the error target with the error
    /// if the `Error?` at `slot` holds one, and otherwise right here.
    fn branch_on_error(&mut self, slot: Value, on_error: &ErrorTarget) -> Result<(), CodegenError> {
        let raised = self.builder.create_block();
        let fine = self.builder.create_block();
        let tag = self.emitter().tag(slot);
        self.builder.ins().brif(tag, raised, &[], fine, &[]);
        self.builder.switch_to_block(raised);
        let offset = self
            .ctx
            .layouts
            .variant_field_offset(self.error_option(), 1, 0)
            .ok_or_else(|| CodegenError("no layout for errors".to_owned()))?;
        let error = self.offset_address(slot, offset);
        let dest = self.place_address(&on_error.place)?;
        self.copy_value(dest, error, self.program.types.error_ty());
        self.builder.ins().jump(self.blocks[on_error.block], &[]);
        self.builder.switch_to_block(fine);
        Ok(())
    }

    /// `Raise`: moves the error into the caller's `Error?`, and returns.
    fn raise(&mut self, error: &Operand) -> Result<(), CodegenError> {
        let source = self.operand_address(error)?;
        let slot = self
            .builder
            .use_var(self.error_pointer.expect("set in functions that raise"));
        let option = self.error_option();
        self.emitter().store_some(slot, option, source)?;
        // The result is not used: any value of its type will do.
        match self
            .ctx
            .layouts
            .repr(self.body.locals[self.body.return_local].ty)
        {
            Repr::Scalar(ty) => {
                let zero = self.zero(ty);
                self.builder.ins().return_(&[zero]);
            }
            Repr::Memory { .. } | Repr::None => {
                self.builder.ins().return_(&[]);
            }
        }
        Ok(())
    }
}

/// The type of the elements of a list type.
fn element_of(list: Ty) -> Ty {
    match list {
        Ty::List(element) => *element,
        _ => Ty::Error,
    }
}

/// The integer condition code for a comparison.
fn int_condition(op: BinaryOp, signed: bool) -> IntCC {
    match (op, signed) {
        (BinaryOp::Eq, _) => IntCC::Equal,
        (BinaryOp::Ne, _) => IntCC::NotEqual,
        (BinaryOp::Lt, true) => IntCC::SignedLessThan,
        (BinaryOp::Lt, false) => IntCC::UnsignedLessThan,
        (BinaryOp::Le, true) => IntCC::SignedLessThanOrEqual,
        (BinaryOp::Le, false) => IntCC::UnsignedLessThanOrEqual,
        (BinaryOp::Gt, true) => IntCC::SignedGreaterThan,
        (BinaryOp::Gt, false) => IntCC::UnsignedGreaterThan,
        (BinaryOp::Ge, true) => IntCC::SignedGreaterThanOrEqual,
        (BinaryOp::Ge, false) => IntCC::UnsignedGreaterThanOrEqual,
        _ => unreachable!("not a comparison: {op:?}"),
    }
}
