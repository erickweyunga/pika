//! Function values (spec section 11.4).
//!
//! A function value is a pointer to a heap object shared by its copies:
//!
//! | Offset | Content |
//! |---|---|
//! | 0 | the number of copies (`u64`) |
//! | 8 | the code of the body, which takes the object as its first argument |
//! | 16 | the function that destroys the captured values, or null if none needs it |
//! | 24 | the size of the object in bytes |
//! | 32 | the captured values, laid out in order like the fields of a struct |
//!
//! Cloning a function value increments the count; destroying one decrements it, and the
//! last copy destroys the captured values and frees the object.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{AbiParam, InstBuilder, MemFlagsData, Value, types};
use cranelift_frontend::FunctionBuilder;
use cranelift_module::{FuncId, Linkage, Module};
use pika_mir::InstanceId;
use pika_types::Ty;

use crate::glue::Emitter;
use crate::{CodegenError, ModuleCtx, error};

/// Where the number of copies is.
const COUNT: i32 = 0;
/// Where the code is.
pub(crate) const CODE: i32 = 8;
/// Where the function that destroys the captured values is.
const DROP: i32 = 16;
/// Where the size of the object is.
const SIZE: i32 = 24;
/// Where the captured values start.
pub(crate) const CAPTURES: u32 = 32;
/// The alignment of function value objects.
const ALIGN: u64 = 8;

impl ModuleCtx {
    /// The function that destroys the captured values of the function values of body
    /// `instance`, whose captures have the types `captures`; `None` if none needs it.
    pub(crate) fn env_drop(
        &mut self,
        module: &mut dyn Module,
        instance: InstanceId,
        captures: &[Ty],
    ) -> Result<Option<FuncId>, CodegenError> {
        if let Some(&known) = self.env_drops.get(&instance) {
            return Ok(known);
        }
        let needed = captures.iter().any(|&ty| self.layouts.needs_drop(ty));
        let func = if needed {
            let name = format!("pika_env_drop_{}", instance.into_raw().into_u32());
            let func = module
                .declare_function(&name, Linkage::Local, &env_drop_signature(module))
                .map_err(|e| error("declaring a capture destructor", e))?;
            self.pending_env_drops.push((func, captures.to_vec()));
            Some(func)
        } else {
            None
        };
        self.env_drops.insert(instance, func);
        Ok(func)
    }
}

/// The signature of the functions that destroy captured values: they take the object.
fn env_drop_signature(module: &dyn Module) -> cranelift_codegen::ir::Signature {
    let mut signature = module.make_signature();
    signature
        .params
        .push(AbiParam::new(module.target_config().pointer_type()));
    signature
}

/// Defines the capture destructors declared so far.
pub(crate) fn define_env_drop(
    emitter: &mut Emitter<'_, '_>,
    object: Value,
    captures: &[Ty],
) -> Result<(), CodegenError> {
    let (offsets, _) = emitter.ctx.layouts.env_layout(captures);
    for (&ty, offset) in captures.iter().zip(offsets) {
        let address = emitter.offset(object, offset);
        emitter.drop_value(address, ty)?;
    }
    Ok(())
}

/// Declares the signature of capture destructors in the function being built.
pub(crate) fn env_drop_signature_ref(
    builder: &mut FunctionBuilder<'_>,
    module: &dyn Module,
) -> cranelift_codegen::ir::SigRef {
    builder.import_signature(env_drop_signature(module))
}

impl Emitter<'_, '_> {
    /// A new function value with body `code` and capture destructor `drop`, whose captures
    /// take `size` bytes; the captures are left for the caller to store.
    pub(crate) fn new_function_value(
        &mut self,
        code: FuncId,
        drop: Option<FuncId>,
        size: u32,
    ) -> Value {
        let size_value = self.iconst(self.pointer, u64::from(size));
        let align = self.iconst(self.pointer, ALIGN);
        let object = self
            .call(self.ctx.runtime.alloc, &[size_value, align])
            .expect("`pika_alloc` returns a pointer");
        let flags = MemFlagsData::trusted();
        let one = self.iconst(types::I64, 1);
        self.builder.ins().store(flags, one, object, COUNT);
        let code_ref = self.module.declare_func_in_func(code, self.builder.func);
        let code_address = self.builder.ins().func_addr(self.pointer, code_ref);
        self.builder.ins().store(flags, code_address, object, CODE);
        let drop_address = match drop {
            Some(drop) => {
                let drop_ref = self.module.declare_func_in_func(drop, self.builder.func);
                self.builder.ins().func_addr(self.pointer, drop_ref)
            }
            None => self.iconst(self.pointer, 0),
        };
        self.builder.ins().store(flags, drop_address, object, DROP);
        self.builder.ins().store(flags, size_value, object, SIZE);
        object
    }

    /// Destroys the function value at `address`: one copy fewer, and the last one destroys
    /// the captures and frees the object.
    pub(crate) fn drop_function_value(&mut self, address: Value) {
        let object = self
            .builder
            .ins()
            .load(self.pointer, MemFlagsData::trusted(), address, 0);
        self.drop_function_object(object);
    }

    /// Destroys a copy of the function value whose object is `object`.
    pub(crate) fn drop_function_object(&mut self, object: Value) {
        let flags = MemFlagsData::trusted();
        let count = self.builder.ins().load(types::I64, flags, object, COUNT);
        let count = self.builder.ins().iadd_imm_s(count, -1);
        self.builder.ins().store(flags, count, object, COUNT);
        let (last, done) = (self.builder.create_block(), self.builder.create_block());
        let is_last = self.builder.ins().icmp_imm_s(IntCC::Equal, count, 0);
        self.builder.ins().brif(is_last, last, &[], done, &[]);

        self.builder.switch_to_block(last);
        let drop = self.builder.ins().load(self.pointer, flags, object, DROP);
        let (call_drop, free) = (self.builder.create_block(), self.builder.create_block());
        self.builder.ins().brif(drop, call_drop, &[], free, &[]);
        self.builder.switch_to_block(call_drop);
        let signature = env_drop_signature_ref(self.builder, self.module);
        self.builder.ins().call_indirect(signature, drop, &[object]);
        self.builder.ins().jump(free, &[]);
        self.builder.switch_to_block(free);
        let size = self.builder.ins().load(self.pointer, flags, object, SIZE);
        let align = self.iconst(self.pointer, ALIGN);
        self.call(self.ctx.runtime.free, &[object, size, align]);
        self.builder.ins().jump(done, &[]);

        self.builder.switch_to_block(done);
    }

    /// Copies the function value at `source` into `dest`: one copy more.
    pub(crate) fn clone_function_value(&mut self, dest: Value, source: Value) {
        let flags = MemFlagsData::trusted();
        let object = self.builder.ins().load(self.pointer, flags, source, 0);
        let object = self.clone_function_object(object);
        self.builder.ins().store(flags, object, dest, 0);
    }

    /// A new copy of the function value whose object is `object`: the same object.
    pub(crate) fn clone_function_object(&mut self, object: Value) -> Value {
        let flags = MemFlagsData::trusted();
        let count = self.builder.ins().load(types::I64, flags, object, COUNT);
        let count = self.builder.ins().iadd_imm_s(count, 1);
        self.builder.ins().store(flags, count, object, COUNT);
        object
    }
}
