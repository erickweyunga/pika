//! Operations on values of every type: destruction, cloning, comparison and display.
//!
//! [`Emitter`] emits these operations into any function being built, for function bodies
//! and for the generated functions below. Options and boxes are handled inline. Structs,
//! enums and collections get generated functions ("glue"), one per type and operation, which
//! the emitted code calls: types can contain themselves through a box, so their operations
//! recurse, and collections loop over their elements. Maps also get a function comparing
//! two keys, which the runtime calls. Glue is declared when first needed and defined once all
//! code that needs it is known ([`define_pending`]).

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{AbiParam, Block, InstBuilder, MemFlagsData, Type, Value, types};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{FuncId, Linkage, Module};
use pika_hir::{FloatTy, IntTy};
use pika_types::Ty;

use crate::repr::{self, LayoutKind, Repr};
use crate::{CodegenError, ModuleCtx, error};

/// Ordering codes produced by comparisons, as `i8` values.
pub(crate) const EQUAL: u64 = 0;
pub(crate) const LESS: u64 = 1;
pub(crate) const GREATER: u64 = 2;
const UNORDERED: u64 = 3;

/// A value ready to be used: a scalar, or the address of a value held in memory.
#[derive(Clone, Copy)]
pub(crate) enum Loaded {
    Scalar(Value),
    Address(Value),
    /// A value without representation.
    Nothing,
}

/// The operations that structs and enums get generated functions for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum GlueKind {
    /// `drop(value: *mut T)`
    Drop,
    /// `clone(dest: *mut T, source: *const T)`, into uninitialized memory.
    Clone,
    /// `compare(a: *const T, b: *const T) -> i8`, an ordering code.
    Compare,
    /// `display(value: *const T, dest: *mut String)`, appending the text.
    Display,
    /// `eq(a: *const T, b: *const T) -> u8`: whether two keys of a map are equal.
    KeyEq,
    /// `hash(value: *const T) -> u64`, for keys.
    Hash,
}

impl GlueKind {
    fn name(self) -> &'static str {
        match self {
            Self::Drop => "drop",
            Self::Clone => "clone",
            Self::Compare => "compare",
            Self::Display => "display",
            Self::KeyEq => "key_eq",
            Self::Hash => "hash",
        }
    }

    fn signature(self, module: &dyn Module) -> cranelift_codegen::ir::Signature {
        let pointer = module.target_config().pointer_type();
        let mut signature = module.make_signature();
        let params = if matches!(self, Self::Drop | Self::Hash) {
            1
        } else {
            2
        };
        for _ in 0..params {
            signature.params.push(AbiParam::new(pointer));
        }
        match self {
            Self::Compare | Self::KeyEq => signature.returns.push(AbiParam::new(types::I8)),
            Self::Hash => signature.returns.push(AbiParam::new(types::I64)),
            Self::Drop | Self::Clone | Self::Display => {}
        }
        signature
    }
}

impl ModuleCtx {
    /// The glue function of a type for an operation, declared on first use: of a struct,
    /// enum or collection, or the key comparison of any type.
    pub(crate) fn glue(
        &mut self,
        module: &mut dyn Module,
        ty: Ty,
        kind: GlueKind,
    ) -> Result<FuncId, CodegenError> {
        if let Some(&func) = self.glue.get(&(ty, kind)) {
            return Ok(func);
        }
        let has_glue = match ty {
            Ty::Adt(_) => true,
            Ty::List(_) | Ty::Map(_) | Ty::Set(_) => kind != GlueKind::Hash,
            _ => kind == GlueKind::KeyEq,
        };
        if !has_glue {
            return Err(CodegenError(format!("no {} glue for {ty}", kind.name())));
        }
        let name = format!("pika_glue_{}_{}", kind.name(), self.glue.len());
        let func = module
            .declare_function(&name, Linkage::Local, &kind.signature(module))
            .map_err(|e| error("declaring glue", e))?;
        self.glue.insert((ty, kind), func);
        self.pending_glue.push((ty, kind, func));
        Ok(func)
    }
}

/// Defines the glue functions declared so far, and those they need in turn.
pub(crate) fn define_pending(
    module: &mut dyn Module,
    ctx: &mut ModuleCtx,
    context: &mut cranelift_codegen::Context,
    builder_context: &mut FunctionBuilderContext,
) -> Result<(), CodegenError> {
    loop {
        if let Some((func, captures)) = ctx.pending_env_drops.pop() {
            define_env_drop_function(module, ctx, context, builder_context, func, &captures)?;
            continue;
        }
        let Some((ty, kind, func)) = ctx.pending_glue.pop() else {
            break;
        };
        context.func.signature = kind.signature(module);
        {
            let mut builder = FunctionBuilder::new(&mut context.func, builder_context);
            let entry = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.switch_to_block(entry);
            let params = builder.block_params(entry).to_vec();
            let pointer = module.target_config().pointer_type();
            let mut emitter = Emitter {
                builder: &mut builder,
                module,
                ctx,
                pointer,
            };
            let result = emitter.define_glue(ty, kind, &params)?;
            let returned: Vec<Value> = result.into_iter().collect();
            builder.ins().return_(&returned);
            builder.seal_all_blocks();
            builder.finalize(module.target_config());
        }
        module
            .define_function(func, context)
            .map_err(|e| CodegenError(format!("compiling {} glue of {ty}: {e:?}", kind.name())))?;
        module.clear_context(context);
    }
    Ok(())
}

/// Defines `func`, which destroys captured values of the types `captures` in the function
/// value object it receives.
fn define_env_drop_function(
    module: &mut dyn Module,
    ctx: &mut ModuleCtx,
    context: &mut cranelift_codegen::Context,
    builder_context: &mut FunctionBuilderContext,
    func: FuncId,
    captures: &[Ty],
) -> Result<(), CodegenError> {
    let pointer = module.target_config().pointer_type();
    let mut signature = module.make_signature();
    signature.params.push(AbiParam::new(pointer));
    context.func.signature = signature;
    {
        let mut builder = FunctionBuilder::new(&mut context.func, builder_context);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        let object = builder.block_params(entry)[0];
        let mut emitter = Emitter {
            builder: &mut builder,
            module,
            ctx,
            pointer,
        };
        crate::functions::define_env_drop(&mut emitter, object, captures)?;
        builder.ins().return_(&[]);
        builder.seal_all_blocks();
        builder.finalize(module.target_config());
    }
    module
        .define_function(func, context)
        .map_err(|e| CodegenError(format!("compiling a capture destructor: {e:?}")))?;
    module.clear_context(context);
    Ok(())
}

/// Emits operations on values into a function being built.
pub(crate) struct Emitter<'a, 'f> {
    pub(crate) builder: &'a mut FunctionBuilder<'f>,
    pub(crate) module: &'a mut dyn Module,
    pub(crate) ctx: &'a mut ModuleCtx,
    pub(crate) pointer: Type,
}

impl Emitter<'_, '_> {
    // ----- Helpers -----------------------------------------------------------------------

    /// An integer constant of type `ty` from its bit pattern.
    pub(crate) fn iconst(&mut self, ty: Type, bits: u64) -> Value {
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

    /// Calls a function and returns its result, if any.
    pub(crate) fn call(&mut self, func: FuncId, args: &[Value]) -> Option<Value> {
        let func_ref = self.module.declare_func_in_func(func, self.builder.func);
        let call = self.builder.ins().call(func_ref, args);
        self.builder.inst_results(call).first().copied()
    }

    /// `base + offset`
    pub(crate) fn offset(&mut self, base: Value, offset: u32) -> Value {
        if offset == 0 {
            return base;
        }
        let offset = self.builder.ins().iconst(self.pointer, i64::from(offset));
        self.builder.ins().iadd(base, offset)
    }

    /// The address and length of static text.
    pub(crate) fn text(&mut self, text: &str) -> Result<(Value, Value), CodegenError> {
        let data = self.ctx.string_data(self.module, text)?;
        let global = self.module.declare_data_in_func(data, self.builder.func);
        let address = self.builder.ins().symbol_value(self.pointer, global);
        let length = self.iconst(self.pointer, text.len() as u64);
        Ok((address, length))
    }

    /// Appends static text to the string at `dest`.
    pub(crate) fn push_text(&mut self, dest: Value, text: &str) -> Result<(), CodegenError> {
        if text.is_empty() {
            return Ok(());
        }
        let (address, length) = self.text(text)?;
        self.call(self.ctx.runtime.string_push_bytes, &[dest, address, length]);
        Ok(())
    }

    /// The value of type `ty` at `address`: loaded if it is a scalar.
    pub(crate) fn load_value(&mut self, address: Value, ty: Ty) -> Loaded {
        match self.ctx.layouts.repr(ty) {
            Repr::Scalar(scalar) => Loaded::Scalar(self.builder.ins().load(
                scalar,
                MemFlagsData::trusted(),
                address,
                0,
            )),
            Repr::Memory { .. } => Loaded::Address(address),
            Repr::None => Loaded::Nothing,
        }
    }

    pub(crate) fn copy_memory(&mut self, dest: Value, source: Value, size: u32, align: u8) {
        let config = self.module.target_config();
        self.builder.emit_small_memory_copy(
            config,
            dest,
            source,
            u64::from(size),
            align,
            align,
            true,
            MemFlagsData::trusted(),
        );
    }

    /// The tag of the enum or option at `address`: the index of its variant.
    pub(crate) fn tag(&mut self, address: Value) -> Value {
        self.builder
            .ins()
            .load(types::I32, MemFlagsData::trusted(), address, 0)
    }

    /// Runs `f` for each listed variant, in a block reached only when the tag is that
    /// variant; all paths continue after.
    fn for_variant(
        &mut self,
        tag: Value,
        variants: &[u32],
        mut f: impl FnMut(&mut Self, u32) -> Result<(), CodegenError>,
    ) -> Result<(), CodegenError> {
        let done = self.builder.create_block();
        for &variant in variants {
            let then_block = self.builder.create_block();
            let next = self.builder.create_block();
            let index = self.iconst(types::I32, u64::from(variant));
            let is = self.builder.ins().icmp(IntCC::Equal, tag, index);
            self.builder.ins().brif(is, then_block, &[], next, &[]);
            self.builder.switch_to_block(then_block);
            f(self, variant)?;
            self.builder.ins().jump(done, &[]);
            self.builder.switch_to_block(next);
        }
        self.builder.ins().jump(done, &[]);
        self.builder.switch_to_block(done);
        Ok(())
    }

    // ----- Destruction and cloning -------------------------------------------------------

    /// Destroys the value of type `ty` at `address`.
    pub(crate) fn drop_value(&mut self, address: Value, ty: Ty) -> Result<(), CodegenError> {
        if !self.ctx.layouts.needs_drop(ty) {
            return Ok(());
        }
        match ty {
            Ty::String | Ty::Formatter => {
                self.call(self.ctx.runtime.string_drop, &[address]);
            }
            Ty::Fn(_) => self.drop_function_value(address),
            Ty::Box(inner) => {
                let value = self.load_pointer(address);
                self.drop_box(value, *inner)?;
            }
            Ty::Option(inner) => {
                let offset = self.some_offset(ty)?;
                let tag = self.tag(address);
                self.for_variant(tag, &[1], |this, _| {
                    let value = this.offset(address, offset);
                    this.drop_value(value, *inner)
                })?;
            }
            _ => {
                let glue = self.ctx.glue(self.module, ty, GlueKind::Drop)?;
                self.call(glue, &[address]);
            }
        }
        Ok(())
    }

    /// Destroys the value of type `inner` in the box `value`, and frees the box.
    pub(crate) fn drop_box(&mut self, value: Value, inner: Ty) -> Result<(), CodegenError> {
        self.drop_value(value, inner)?;
        self.free(value, inner);
        Ok(())
    }

    /// Writes a copy of the value of type `ty` at `source` into the memory at `dest`.
    pub(crate) fn clone_value(
        &mut self,
        dest: Value,
        source: Value,
        ty: Ty,
    ) -> Result<(), CodegenError> {
        match ty {
            Ty::String => {
                self.call(self.ctx.runtime.string_clone, &[dest, source]);
            }
            Ty::Fn(_) => self.clone_function_value(dest, source),
            Ty::Box(inner) => {
                let value = self.load_pointer(source);
                let copy = self.clone_box(value, *inner)?;
                self.builder
                    .ins()
                    .store(MemFlagsData::trusted(), copy, dest, 0);
            }
            Ty::Option(inner) if self.ctx.layouts.needs_drop(ty) => {
                self.copy_bytes(dest, source, ty);
                let offset = self.some_offset(ty)?;
                let tag = self.tag(source);
                self.for_variant(tag, &[1], |this, _| {
                    let (dest, source) = (this.offset(dest, offset), this.offset(source, offset));
                    this.clone_value(dest, source, *inner)
                })?;
            }
            _ if self.ctx.layouts.needs_drop(ty) => {
                let glue = self.ctx.glue(self.module, ty, GlueKind::Clone)?;
                self.call(glue, &[dest, source]);
            }
            _ => self.copy_bytes(dest, source, ty),
        }
        Ok(())
    }

    /// A new box holding a copy of the value of type `inner` in the box `value`.
    pub(crate) fn clone_box(&mut self, value: Value, inner: Ty) -> Result<Value, CodegenError> {
        let copy = self.alloc(inner);
        self.clone_value(copy, value, inner)?;
        Ok(copy)
    }

    /// Copies the bytes of a value of type `ty`.
    pub(crate) fn copy_bytes(&mut self, dest: Value, source: Value, ty: Ty) {
        match self.ctx.layouts.repr(ty) {
            Repr::Scalar(scalar) => {
                let flags = MemFlagsData::trusted();
                let value = self.builder.ins().load(scalar, flags, source, 0);
                self.builder.ins().store(flags, value, dest, 0);
            }
            Repr::Memory { size, align } => self.copy_memory(dest, source, size, align),
            Repr::None => {}
        }
    }

    fn load_pointer(&mut self, address: Value) -> Value {
        self.builder
            .ins()
            .load(self.pointer, MemFlagsData::trusted(), address, 0)
    }

    /// Allocates heap memory for a value of type `ty`.
    pub(crate) fn alloc(&mut self, ty: Ty) -> Value {
        let (size, align) = self.ctx.layouts.size_align(ty);
        let size = self.iconst(self.pointer, u64::from(size));
        let align = self.iconst(self.pointer, u64::from(align));
        self.call(self.ctx.runtime.alloc, &[size, align])
            .expect("`pika_alloc` returns a pointer")
    }

    /// Frees heap memory allocated by [`Emitter::alloc`] for a value of type `ty`.
    pub(crate) fn free(&mut self, value: Value, ty: Ty) {
        let (size, align) = self.ctx.layouts.size_align(ty);
        let size = self.iconst(self.pointer, u64::from(size));
        let align = self.iconst(self.pointer, u64::from(align));
        self.call(self.ctx.runtime.free, &[value, size, align]);
    }

    /// The offset of the value of a `some` of option type `ty`.
    fn some_offset(&self, ty: Ty) -> Result<u32, CodegenError> {
        self.ctx
            .layouts
            .variant_field_offset(ty, 1, 0)
            .ok_or_else(|| CodegenError(format!("no layout for {ty}")))
    }

    // ----- Comparison --------------------------------------------------------------------

    /// Compares two values of type `ty`: an `i8` ordering code. Structs compare field by
    /// field in declaration order; enums and options by variant, then field by field.
    pub(crate) fn compare(&mut self, ty: Ty, a: Loaded, b: Loaded) -> Result<Value, CodegenError> {
        match (ty, a, b) {
            (Ty::String, Loaded::Address(a), Loaded::Address(b)) => {
                let ordering = self
                    .call(self.ctx.runtime.string_compare, &[a, b])
                    .expect("returns a value");
                let zero = self.iconst(types::I32, 0);
                let less = self
                    .builder
                    .ins()
                    .icmp(IntCC::SignedLessThan, ordering, zero);
                let greater = self
                    .builder
                    .ins()
                    .icmp(IntCC::SignedGreaterThan, ordering, zero);
                Ok(self.ordering_code(less, greater, None))
            }
            (Ty::Box(inner), Loaded::Scalar(a), Loaded::Scalar(b)) => {
                let (a, b) = (self.load_value(a, *inner), self.load_value(b, *inner));
                self.compare(*inner, a, b)
            }
            (Ty::Option(inner), Loaded::Address(a), Loaded::Address(b)) => {
                let offset = self.some_offset(ty)?;
                self.compare_variants(a, b, &[(1, vec![(*inner, offset)])])
            }
            (_, Loaded::Address(a), Loaded::Address(b)) => {
                let glue = self.ctx.glue(self.module, ty, GlueKind::Compare)?;
                Ok(self.call(glue, &[a, b]).expect("returns an ordering"))
            }
            (_, Loaded::Scalar(a), Loaded::Scalar(b)) => Ok(self.compare_scalars(ty, a, b)),
            _ => Ok(self.iconst(types::I8, EQUAL)),
        }
    }

    fn compare_scalars(&mut self, ty: Ty, a: Value, b: Value) -> Value {
        if let Ty::Float(_) = ty {
            let less = self.builder.ins().fcmp(FloatCC::LessThan, a, b);
            let greater = self.builder.ins().fcmp(FloatCC::GreaterThan, a, b);
            let equal = self.builder.ins().fcmp(FloatCC::Equal, a, b);
            return self.ordering_code(less, greater, Some(equal));
        }
        let signed = match ty {
            Ty::Int(int) => int.is_signed(),
            _ => ty == Ty::Duration,
        };
        let (less_cc, greater_cc) = if signed {
            (IntCC::SignedLessThan, IntCC::SignedGreaterThan)
        } else {
            (IntCC::UnsignedLessThan, IntCC::UnsignedGreaterThan)
        };
        let less = self.builder.ins().icmp(less_cc, a, b);
        let greater = self.builder.ins().icmp(greater_cc, a, b);
        self.ordering_code(less, greater, None)
    }

    /// The ordering code from `less` and `greater` tests, and an `equal` test for types with
    /// unordered values.
    fn ordering_code(&mut self, less: Value, greater: Value, equal: Option<Value>) -> Value {
        let otherwise = match equal {
            Some(equal) => {
                let equal_code = self.iconst(types::I8, EQUAL);
                let unordered = self.iconst(types::I8, UNORDERED);
                self.builder.ins().select(equal, equal_code, unordered)
            }
            None => self.iconst(types::I8, EQUAL),
        };
        let greater_code = self.iconst(types::I8, GREATER);
        let not_less = self.builder.ins().select(greater, greater_code, otherwise);
        let less_code = self.iconst(types::I8, LESS);
        self.builder.ins().select(less, less_code, not_less)
    }

    /// Compares fields of the values at `a` and `b` in order, each given by its type and
    /// offset: the first pair that is not equal decides. Jumps to `done` with the result.
    fn compare_fields(
        &mut self,
        a: Value,
        b: Value,
        fields: &[(Ty, u32)],
        done: Block,
    ) -> Result<(), CodegenError> {
        for &(ty, offset) in fields {
            let (field_a, field_b) = (self.offset(a, offset), self.offset(b, offset));
            let (field_a, field_b) = (self.load_value(field_a, ty), self.load_value(field_b, ty));
            let ordering = self.compare(ty, field_a, field_b)?;
            let next = self.builder.create_block();
            self.builder
                .ins()
                .brif(ordering, done, &[ordering.into()], next, &[]);
            self.builder.switch_to_block(next);
        }
        let equal = self.iconst(types::I8, EQUAL);
        self.builder.ins().jump(done, &[equal.into()]);
        Ok(())
    }

    /// Compares two enums or options: by variant, then by the fields of the variant, given
    /// for each variant that has fields.
    fn compare_variants(
        &mut self,
        a: Value,
        b: Value,
        variants: &[(u32, Vec<(Ty, u32)>)],
    ) -> Result<Value, CodegenError> {
        let done = self.builder.create_block();
        let result = self.builder.append_block_param(done, types::I8);
        let (tag_a, tag_b) = (self.tag(a), self.tag(b));
        let less = self
            .builder
            .ins()
            .icmp(IntCC::UnsignedLessThan, tag_a, tag_b);
        let greater = self
            .builder
            .ins()
            .icmp(IntCC::UnsignedGreaterThan, tag_a, tag_b);
        let by_tag = self.ordering_code(less, greater, None);
        let same = self.builder.create_block();
        self.builder
            .ins()
            .brif(by_tag, done, &[by_tag.into()], same, &[]);
        self.builder.switch_to_block(same);
        for (variant, fields) in variants {
            let then_block = self.builder.create_block();
            let next = self.builder.create_block();
            let index = self.iconst(types::I32, u64::from(*variant));
            let is = self.builder.ins().icmp(IntCC::Equal, tag_a, index);
            self.builder.ins().brif(is, then_block, &[], next, &[]);
            self.builder.switch_to_block(then_block);
            self.compare_fields(a, b, fields, done)?;
            self.builder.switch_to_block(next);
        }
        let equal = self.iconst(types::I8, EQUAL);
        self.builder.ins().jump(done, &[equal.into()]);
        self.builder.switch_to_block(done);
        Ok(result)
    }

    // ----- Display -----------------------------------------------------------------------

    /// Appends the text of a value of type `ty` to the string at `dest`; `quoted` writes
    /// strings and characters as in source, as inside a struct.
    pub(crate) fn display(
        &mut self,
        ty: Ty,
        value: Loaded,
        dest: Value,
        quoted: bool,
    ) -> Result<(), CodegenError> {
        let runtime = &self.ctx.runtime;
        let (func, value) = match (ty, value) {
            (Ty::String, Loaded::Address(address)) if quoted => {
                (runtime.string_push_string_quoted, address)
            }
            (Ty::String, Loaded::Address(address)) => (runtime.string_push_string, address),
            (Ty::Box(inner), Loaded::Scalar(pointer)) => {
                let value = self.load_value(pointer, *inner);
                return self.display(*inner, value, dest, quoted);
            }
            (Ty::Option(inner), Loaded::Address(address)) => {
                return self.display_option(*inner, ty, address, dest);
            }
            (Ty::Adt(_) | Ty::List(_) | Ty::Map(_) | Ty::Set(_), value) => {
                let glue = self.ctx.glue(self.module, ty, GlueKind::Display)?;
                let address = match value {
                    Loaded::Address(address) => address,
                    // A struct without fields has no data to point to.
                    _ => self.iconst(self.pointer, 0),
                };
                self.call(glue, &[address, dest]);
                return Ok(());
            }
            (_, Loaded::Scalar(value)) => match ty {
                Ty::Int(int) if int.is_signed() => (
                    runtime.string_push_i64,
                    resize(self.builder, value, int, IntTy::I64),
                ),
                Ty::Int(int) => (
                    runtime.string_push_u64,
                    resize(self.builder, value, int, IntTy::U64),
                ),
                Ty::Float(FloatTy::F64) => (runtime.string_push_f64, value),
                Ty::Float(FloatTy::F32) => (runtime.string_push_f32, value),
                Ty::Bool => (runtime.string_push_bool, value),
                Ty::Char if quoted => (runtime.string_push_char_quoted, value),
                Ty::Char => (runtime.string_push_char, value),
                Ty::Duration => (runtime.string_push_duration, value),
                _ => return Ok(()),
            },
            _ => return Ok(()),
        };
        self.call(func, &[dest, value]);
        Ok(())
    }

    /// `none` or `[some value]`.
    fn display_option(
        &mut self,
        inner: Ty,
        ty: Ty,
        address: Value,
        dest: Value,
    ) -> Result<(), CodegenError> {
        let offset = self.some_offset(ty)?;
        let tag = self.tag(address);
        let (some_block, none_block, done) = (
            self.builder.create_block(),
            self.builder.create_block(),
            self.builder.create_block(),
        );
        self.builder
            .ins()
            .brif(tag, some_block, &[], none_block, &[]);
        self.builder.switch_to_block(some_block);
        self.push_text(dest, "[some ")?;
        let value_address = self.offset(address, offset);
        let value = self.load_value(value_address, inner);
        self.display(inner, value, dest, true)?;
        self.push_text(dest, "]")?;
        self.builder.ins().jump(done, &[]);
        self.builder.switch_to_block(none_block);
        self.push_text(dest, "none")?;
        self.builder.ins().jump(done, &[]);
        self.builder.switch_to_block(done);
        Ok(())
    }

    /// `name=value; ...` for the fields at `address`, with values quoted.
    fn display_fields(
        &mut self,
        address: Value,
        fields: &[(String, Ty, u32)],
        dest: Value,
    ) -> Result<(), CodegenError> {
        for (index, (name, ty, offset)) in fields.iter().enumerate() {
            let separator = if index == 0 { "" } else { "; " };
            self.push_text(dest, &format!("{separator}{name}="))?;
            let field_address = self.offset(address, *offset);
            let value = self.load_value(field_address, *ty);
            self.display(*ty, value, dest, true)?;
        }
        Ok(())
    }

    // ----- Glue --------------------------------------------------------------------------

    /// Emits the body of a glue function; returns its result, if any.
    fn define_glue(
        &mut self,
        ty: Ty,
        kind: GlueKind,
        params: &[Value],
    ) -> Result<Option<Value>, CodegenError> {
        if kind == GlueKind::KeyEq {
            let (a, b) = (
                self.load_value(params[0], ty),
                self.load_value(params[1], ty),
            );
            let ordering = self.compare(ty, a, b)?;
            let equal = self.iconst(types::I8, EQUAL);
            return Ok(Some(self.builder.ins().icmp(IntCC::Equal, ordering, equal)));
        }
        if let Ty::List(_) | Ty::Map(_) | Ty::Set(_) = ty {
            return self.define_collection_glue(ty, kind, params);
        }
        let layout = self
            .ctx
            .layouts
            .layout(ty)
            .ok_or_else(|| CodegenError(format!("no layout for {ty}")))?;
        let struct_fields: Vec<(Ty, u32)> = layout
            .fields()
            .iter()
            .map(|&(_, ty, offset)| (ty, offset))
            .collect();
        // The variants that have fields, with the type and offset of each field.
        let variants: Vec<(u32, Vec<(Ty, u32)>)> = layout
            .variants()
            .iter()
            .enumerate()
            .filter(|(_, (_, fields))| !fields.is_empty())
            .map(|(index, (_, fields))| {
                let fields = fields.iter().map(|&(_, ty, offset)| (ty, offset)).collect();
                (
                    u32::try_from(index).expect("enums have few variants"),
                    fields,
                )
            })
            .collect();
        let indices: Vec<u32> = variants.iter().map(|&(variant, _)| variant).collect();
        let fields_of = |variant: u32| -> &[(Ty, u32)] {
            &variants
                .iter()
                .find(|&&(v, _)| v == variant)
                .expect("a listed variant")
                .1
        };
        match kind {
            GlueKind::Drop => {
                let value = params[0];
                self.call_user_drop(ty, value);
                match &layout.kind {
                    LayoutKind::Struct(_) => self.drop_fields(value, &struct_fields)?,
                    LayoutKind::Enum(_) => {
                        let tag = self.tag(value);
                        self.for_variant(tag, &indices, |this, variant| {
                            this.drop_fields(value, fields_of(variant))
                        })?;
                    }
                }
                Ok(None)
            }
            GlueKind::Clone => {
                let (dest, source) = (params[0], params[1]);
                self.copy_bytes(dest, source, ty);
                match &layout.kind {
                    LayoutKind::Struct(_) => self.clone_fields(dest, source, &struct_fields)?,
                    LayoutKind::Enum(_) => {
                        let tag = self.tag(source);
                        self.for_variant(tag, &indices, |this, variant| {
                            this.clone_fields(dest, source, fields_of(variant))
                        })?;
                    }
                }
                Ok(None)
            }
            GlueKind::Compare => {
                let (a, b) = (params[0], params[1]);
                let result = match &layout.kind {
                    LayoutKind::Struct(_) => {
                        let done = self.builder.create_block();
                        let result = self.builder.append_block_param(done, types::I8);
                        self.compare_fields(a, b, &struct_fields, done)?;
                        self.builder.switch_to_block(done);
                        result
                    }
                    LayoutKind::Enum(_) => self.compare_variants(a, b, &variants)?,
                };
                Ok(Some(result))
            }
            GlueKind::Display => {
                self.define_display(ty, &layout, params)?;
                Ok(None)
            }
            GlueKind::Hash => Ok(Some(self.define_hash_glue(
                &layout.kind,
                params[0],
                &struct_fields,
                &variants,
            )?)),
            GlueKind::KeyEq => unreachable!("handled above"),
        }
    }

    /// Calls the `drop` function of type `ty`, if it has one, on the value at `value`. It runs
    /// before the value's fields are destroyed.
    fn call_user_drop(&mut self, ty: Ty, value: Value) {
        if let Some(&instance) = self.ctx.drop_fns.get(&ty) {
            let (func, _) = self.ctx.functions[instance].clone();
            self.call(func, &[value]);
        }
    }

    /// The body of a display glue function: a call of the type's own `fmt`, or the derived
    /// text.
    fn define_display(
        &mut self,
        ty: Ty,
        layout: &repr::Layout,
        params: &[Value],
    ) -> Result<(), CodegenError> {
        if self.ctx.layouts.is_error(ty) {
            // An error displays as its message, its first field.
            let (_, _, offset) = layout.fields()[0];
            let message = self.offset(params[0], offset);
            self.call(self.ctx.runtime.string_push_string, &[params[1], message]);
            return Ok(());
        }
        let Some(&instance) = self.ctx.display_fns.get(&ty) else {
            return self.define_display_glue(layout, params[0], params[1]);
        };
        let (func, signature) = self.ctx.functions[instance].clone();
        // `self` has no parameter when its type has no representation.
        let args = if signature.params.len() == 2 {
            vec![params[0], params[1]]
        } else {
            vec![params[1]]
        };
        self.call(func, &args);
        Ok(())
    }

    /// The body of a display glue function: `Name{field=value; ...}` for a struct, and
    /// `Name->variant` or `Name->variant{field=value; ...}` for an enum.
    fn define_display_glue(
        &mut self,
        layout: &repr::Layout,
        value: Value,
        dest: Value,
    ) -> Result<(), CodegenError> {
        match &layout.kind {
            LayoutKind::Struct(fields) => {
                self.push_text(dest, &format!("{}{{", layout.name))?;
                self.display_fields(value, fields, dest)?;
                self.push_text(dest, "}")
            }
            LayoutKind::Enum(all) => {
                let tag = self.tag(value);
                let every: Vec<u32> = (0..all.len())
                    .map(|v| u32::try_from(v).expect("enums have few variants"))
                    .collect();
                let name = &layout.name;
                self.for_variant(tag, &every, |this, variant| {
                    let (variant_name, fields) = &all[variant as usize];
                    this.push_text(dest, &format!("{name}->{variant_name}"))?;
                    if !fields.is_empty() {
                        this.push_text(dest, "{")?;
                        this.display_fields(value, fields, dest)?;
                        this.push_text(dest, "}")?;
                    }
                    Ok(())
                })
            }
        }
    }

    /// The body of a hash glue function of a struct or enum.
    fn define_hash_glue(
        &mut self,
        kind: &LayoutKind,
        value: Value,
        struct_fields: &[(Ty, u32)],
        variants: &[(u32, Vec<(Ty, u32)>)],
    ) -> Result<Value, CodegenError> {
        let zero = self.iconst(types::I64, 0);
        let result = match kind {
            LayoutKind::Struct(_) => self.hash_fields(value, struct_fields, zero)?,
            LayoutKind::Enum(_) => {
                let done = self.builder.create_block();
                let result = self.builder.append_block_param(done, types::I64);
                let tag = self.tag(value);
                let tag = self.builder.ins().uextend(types::I64, tag);
                let by_tag = self.hash_combine(zero, tag);
                for (variant, fields) in variants {
                    let (then_block, next) =
                        (self.builder.create_block(), self.builder.create_block());
                    let index = self.iconst(types::I64, u64::from(*variant));
                    let is = self.builder.ins().icmp(IntCC::Equal, tag, index);
                    self.builder.ins().brif(is, then_block, &[], next, &[]);
                    self.builder.switch_to_block(then_block);
                    let hash = self.hash_fields(value, fields, by_tag)?;
                    self.builder.ins().jump(done, &[hash.into()]);
                    self.builder.switch_to_block(next);
                }
                self.builder.ins().jump(done, &[by_tag.into()]);
                self.builder.switch_to_block(done);
                result
            }
        };
        Ok(result)
    }

    /// `hash` combined with the hashes of the fields at `value`.
    fn hash_fields(
        &mut self,
        value: Value,
        fields: &[(Ty, u32)],
        mut hash: Value,
    ) -> Result<Value, CodegenError> {
        for &(ty, offset) in fields {
            let address = self.offset(value, offset);
            let field = self.load_value(address, ty);
            let field_hash = self.hash(ty, field)?;
            hash = self.hash_combine(hash, field_hash);
        }
        Ok(hash)
    }

    fn drop_fields(&mut self, value: Value, fields: &[(Ty, u32)]) -> Result<(), CodegenError> {
        for &(ty, offset) in fields {
            if self.ctx.layouts.needs_drop(ty) {
                let address = self.offset(value, offset);
                self.drop_value(address, ty)?;
            }
        }
        Ok(())
    }

    fn clone_fields(
        &mut self,
        dest: Value,
        source: Value,
        fields: &[(Ty, u32)],
    ) -> Result<(), CodegenError> {
        for &(ty, offset) in fields {
            if self.ctx.layouts.needs_drop(ty) {
                let (dest, source) = (self.offset(dest, offset), self.offset(source, offset));
                self.clone_value(dest, source, ty)?;
            }
        }
        Ok(())
    }
}

/// Converts between integer widths: extends according to the source's signedness, or keeps
/// the low bits.
pub(crate) fn resize(builder: &mut FunctionBuilder, value: Value, from: IntTy, to: IntTy) -> Value {
    let to_cl = repr::int_type(to);
    match to.bits().cmp(&from.bits()) {
        std::cmp::Ordering::Greater if from.is_signed() => builder.ins().sextend(to_cl, value),
        std::cmp::Ordering::Greater => builder.ins().uextend(to_cl, value),
        std::cmp::Ordering::Less => builder.ins().ireduce(to_cl, value),
        std::cmp::Ordering::Equal => value,
    }
}
