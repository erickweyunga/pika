//! Lists, maps and sets in generated code.
//!
//! The runtime (`pika_runtime::collections`) manages the buffers; the code generated here
//! reads and writes the elements, hashes keys, and destroys, clones, compares and displays
//! collections element by element. A map's entry is a `u64` hash, its key and its value; a
//! set is a map whose values have no data.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{InstBuilder, MemFlagsData, Value, types};
use pika_types::Ty;

use crate::CodegenError;
use crate::glue::{EQUAL, Emitter, GREATER, GlueKind, LESS, Loaded};
use crate::repr::{EntryLayout, LEN_OFFSET, MAP_SIZE, STRING_SIZE};

/// The multiplier of the hash combination (from the "Fx" hash).
const HASH_MULTIPLIER: u64 = 0x517c_c1b7_2722_0a95;

impl Emitter<'_, '_> {
    // ----- Buffers -----------------------------------------------------------------------

    /// `value + amount`, for `i64` values.
    fn add_int(&mut self, value: Value, amount: i64) -> Value {
        let amount = self.builder.ins().iconst(types::I64, amount);
        self.builder.ins().iadd(value, amount)
    }

    /// The number of elements of the list, map or set at `collection`.
    pub(crate) fn len(&mut self, collection: Value) -> Value {
        self.builder
            .ins()
            .load(types::I64, MemFlagsData::trusted(), collection, LEN_OFFSET)
    }

    fn set_len(&mut self, collection: Value, len: Value) {
        self.builder
            .ins()
            .store(MemFlagsData::trusted(), len, collection, LEN_OFFSET);
    }

    /// An empty list, map or set of type `ty` at `address`.
    pub(crate) fn init_collection(&mut self, address: Value, ty: Ty) {
        let size = if let Ty::List(_) = ty {
            STRING_SIZE
        } else {
            MAP_SIZE
        };
        let zero = self.iconst(types::I64, 0);
        for offset in (0..size).step_by(8) {
            let offset = i32::try_from(offset).expect("small offsets");
            self.builder
                .ins()
                .store(MemFlagsData::trusted(), zero, address, offset);
        }
    }

    /// The size and alignment of elements of type `ty`, as pointer-sized values.
    fn element_size(&mut self, ty: Ty) -> (Value, Value) {
        let (size, align) = self.ctx.layouts.size_align(ty);
        (
            self.iconst(self.pointer, u64::from(size)),
            self.iconst(self.pointer, u64::from(align)),
        )
    }

    /// The address of element `index` of the list at `list`, with elements of type `ty`.
    pub(crate) fn element_addr(&mut self, list: Value, index: Value, ty: Ty) -> Value {
        let (size, _) = self.ctx.layouts.size_align(ty);
        let base = self
            .builder
            .ins()
            .load(self.pointer, MemFlagsData::trusted(), list, 0);
        let size = self.iconst(self.pointer, u64::from(size));
        let offset = self.builder.ins().imul(index, size);
        self.builder.ins().iadd(base, offset)
    }

    /// The address of entry `index` of the map at `map`.
    pub(crate) fn entry_addr(&mut self, map: Value, index: Value, entry: &EntryLayout) -> Value {
        let base = self
            .builder
            .ins()
            .load(self.pointer, MemFlagsData::trusted(), map, 0);
        let size = self.iconst(self.pointer, u64::from(entry.size));
        let offset = self.builder.ins().imul(index, size);
        self.builder.ins().iadd(base, offset)
    }

    /// Makes room in the list at `list` for `additional` more elements of type `ty`.
    pub(crate) fn list_reserve(&mut self, list: Value, additional: Value, ty: Ty) {
        let (size, align) = self.element_size(ty);
        self.call(
            self.ctx.runtime.list_reserve,
            &[list, additional, size, align],
        );
    }

    /// Opens a slot at `index` of the list at `list` and returns its address.
    pub(crate) fn list_open(&mut self, list: Value, index: Value, ty: Ty) -> Value {
        let (size, align) = self.element_size(ty);
        self.call(self.ctx.runtime.list_open, &[list, index, size, align])
            .expect("returns the slot")
    }

    /// Closes the slot at `index` of the list at `list`, whose element was moved out.
    pub(crate) fn list_close(&mut self, list: Value, index: Value, ty: Ty) {
        let (size, _) = self.element_size(ty);
        self.call(self.ctx.runtime.list_close, &[list, index, size]);
    }

    /// The address where a new element goes at the end of the list at `list`: room is
    /// made, and the length raised.
    pub(crate) fn list_push_slot(&mut self, list: Value, ty: Ty) -> Value {
        let len = self.len(list);
        self.list_open(list, len, ty)
    }

    /// Runs `body` for each index from 0 to `len`, exclusive.
    pub(crate) fn for_each(
        &mut self,
        len: Value,
        mut body: impl FnMut(&mut Self, Value) -> Result<(), CodegenError>,
    ) -> Result<(), CodegenError> {
        let header = self.builder.create_block();
        let index = self.builder.append_block_param(header, types::I64);
        let (body_block, done) = (self.builder.create_block(), self.builder.create_block());
        let zero = self.iconst(types::I64, 0);
        self.builder.ins().jump(header, &[zero.into()]);
        self.builder.switch_to_block(header);
        let more = self.builder.ins().icmp(IntCC::SignedLessThan, index, len);
        self.builder.ins().brif(more, body_block, &[], done, &[]);
        self.builder.switch_to_block(body_block);
        body(self, index)?;
        let next = self.add_int(index, 1);
        self.builder.ins().jump(header, &[next.into()]);
        self.builder.switch_to_block(done);
        Ok(())
    }

    /// Runs `test` for each index from 0 to `len` until it gives a true `i8`; returns
    /// whether one did.
    pub(crate) fn search(
        &mut self,
        len: Value,
        mut test: impl FnMut(&mut Self, Value) -> Result<Value, CodegenError>,
    ) -> Result<Value, CodegenError> {
        let header = self.builder.create_block();
        let index = self.builder.append_block_param(header, types::I64);
        let (body_block, next_block, done) = (
            self.builder.create_block(),
            self.builder.create_block(),
            self.builder.create_block(),
        );
        let found = self.builder.append_block_param(done, types::I8);
        let zero = self.iconst(types::I64, 0);
        self.builder.ins().jump(header, &[zero.into()]);
        self.builder.switch_to_block(header);
        let more = self.builder.ins().icmp(IntCC::SignedLessThan, index, len);
        let no = self.iconst(types::I8, 0);
        self.builder
            .ins()
            .brif(more, body_block, &[], done, &[no.into()]);
        self.builder.switch_to_block(body_block);
        let hit = test(self, index)?;
        let yes = self.iconst(types::I8, 1);
        self.builder
            .ins()
            .brif(hit, done, &[yes.into()], next_block, &[]);
        self.builder.switch_to_block(next_block);
        let next = self.add_int(index, 1);
        self.builder.ins().jump(header, &[next.into()]);
        self.builder.switch_to_block(done);
        Ok(found)
    }

    // ----- Hashing -----------------------------------------------------------------------

    /// `hash` combined with `value` (an `i64`).
    pub(crate) fn hash_combine(&mut self, hash: Value, value: Value) -> Value {
        let five = self.iconst(types::I64, 5);
        let rotated = self.builder.ins().rotl(hash, five);
        let mixed = self.builder.ins().bxor(rotated, value);
        let multiplier = self.iconst(types::I64, HASH_MULTIPLIER);
        self.builder.ins().imul(mixed, multiplier)
    }

    /// The hash of a key of type `ty`, as an `i64`.
    pub(crate) fn hash(&mut self, ty: Ty, value: Loaded) -> Result<Value, CodegenError> {
        let zero = self.iconst(types::I64, 0);
        Ok(match (ty, value) {
            (Ty::String, Loaded::Address(address)) => {
                let ptr =
                    self.builder
                        .ins()
                        .load(self.pointer, MemFlagsData::trusted(), address, 0);
                let len = self.len(address);
                self.call(self.ctx.runtime.hash_bytes, &[ptr, len])
                    .expect("returns a hash")
            }
            (Ty::Box(inner), Loaded::Scalar(pointer)) => {
                let value = self.load_value(pointer, *inner);
                self.hash(*inner, value)?
            }
            (Ty::Option(inner), Loaded::Address(address)) => {
                let offset = self
                    .ctx
                    .layouts
                    .variant_field_offset(ty, 1, 0)
                    .ok_or_else(|| CodegenError(format!("no layout for {ty}")))?;
                let tag = self.tag(address);
                let tag = self.builder.ins().uextend(types::I64, tag);
                let by_tag = self.hash_combine(zero, tag);
                let (some_block, done) = (self.builder.create_block(), self.builder.create_block());
                let result = self.builder.append_block_param(done, types::I64);
                self.builder
                    .ins()
                    .brif(tag, some_block, &[], done, &[by_tag.into()]);
                self.builder.switch_to_block(some_block);
                let inner_address = self.offset(address, offset);
                let inner_value = self.load_value(inner_address, *inner);
                let inner_hash = self.hash(*inner, inner_value)?;
                let combined = self.hash_combine(by_tag, inner_hash);
                self.builder.ins().jump(done, &[combined.into()]);
                self.builder.switch_to_block(done);
                result
            }
            (Ty::Adt(_), value) => {
                let glue = self.ctx.glue(self.module, ty, GlueKind::Hash)?;
                let address = match value {
                    Loaded::Address(address) => address,
                    _ => self.iconst(self.pointer, 0),
                };
                self.call(glue, &[address]).expect("returns a hash")
            }
            (Ty::Int(int), Loaded::Scalar(value)) => {
                if int.bits() == 64 {
                    value
                } else if int.is_signed() {
                    self.builder.ins().sextend(types::I64, value)
                } else {
                    self.builder.ins().uextend(types::I64, value)
                }
            }
            (Ty::Bool | Ty::Char, Loaded::Scalar(value)) => {
                self.builder.ins().uextend(types::I64, value)
            }
            (_, Loaded::Scalar(value)) => value,
            _ => zero,
        })
    }

    /// The address of the key comparison function of key type `ty`.
    fn key_eq(&mut self, ty: Ty) -> Result<Value, CodegenError> {
        let func = self.ctx.glue(self.module, ty, GlueKind::KeyEq)?;
        let func_ref = self.module.declare_func_in_func(func, self.builder.func);
        Ok(self.builder.ins().func_addr(self.pointer, func_ref))
    }

    // ----- Maps --------------------------------------------------------------------------

    /// The position of the key at `key` in the map at `map` of type `ty`, or -1, and the
    /// key's hash.
    pub(crate) fn map_find(
        &mut self,
        map: Value,
        ty: Ty,
        key: Value,
    ) -> Result<(Value, Value), CodegenError> {
        let entry = self.ctx.layouts.entry(ty);
        let key_value = self.load_value(key, entry.key);
        let hash = self.hash(entry.key, key_value)?;
        let eq = self.key_eq(entry.key)?;
        let key_offset = self.iconst(self.pointer, u64::from(entry.key_offset));
        let size = self.iconst(self.pointer, u64::from(entry.size));
        let position = self
            .call(
                self.ctx.runtime.map_find,
                &[map, hash, key, eq, key_offset, size],
            )
            .expect("returns a position");
        Ok((position, hash))
    }

    /// Inserts the key at `key` and the value at `value` (both moved) into the map at `map`
    /// of type `ty`. If the key is there, its value is replaced and the new key destroyed.
    /// With `old`, the old value is moved into the option there, or `none` stored.
    pub(crate) fn map_insert(
        &mut self,
        map: Value,
        ty: Ty,
        key: Value,
        value: Value,
        old: Option<(Value, Ty)>,
    ) -> Result<(), CodegenError> {
        let entry = self.ctx.layouts.entry(ty);
        let (position, hash) = self.map_find(map, ty, key)?;
        let (found_block, new_block, done) = (
            self.builder.create_block(),
            self.builder.create_block(),
            self.builder.create_block(),
        );
        let zero = self.iconst(types::I64, 0);
        let found = self
            .builder
            .ins()
            .icmp(IntCC::SignedGreaterThanOrEqual, position, zero);
        self.builder
            .ins()
            .brif(found, found_block, &[], new_block, &[]);

        self.builder.switch_to_block(found_block);
        let existing = self.entry_addr(map, position, &entry);
        let existing_value = self.offset(existing, entry.value_offset);
        match old {
            Some((option, option_ty)) => self.store_some(option, option_ty, existing_value)?,
            None => self.drop_value(existing_value, entry.value)?,
        }
        self.copy_bytes(existing_value, value, entry.value);
        self.drop_value(key, entry.key)?;
        self.builder.ins().jump(done, &[]);

        self.builder.switch_to_block(new_block);
        let size = self.iconst(self.pointer, u64::from(entry.size));
        let align = self.iconst(self.pointer, u64::from(entry.align));
        let slot = self
            .call(self.ctx.runtime.map_push, &[map, hash, size, align])
            .expect("returns the entry");
        let (slot_key, slot_value) = (
            self.offset(slot, entry.key_offset),
            self.offset(slot, entry.value_offset),
        );
        self.copy_bytes(slot_key, key, entry.key);
        self.copy_bytes(slot_value, value, entry.value);
        if let Some((option, _)) = old {
            self.store_none(option);
        }
        self.builder.ins().jump(done, &[]);
        self.builder.switch_to_block(done);
        Ok(())
    }

    /// Removes the entry with the key at `key` from the map at `map`: its key is destroyed
    /// and its value moved into the option at `option`, or `none` stored.
    pub(crate) fn map_remove(
        &mut self,
        map: Value,
        ty: Ty,
        key: Value,
        option: Value,
        option_ty: Ty,
    ) -> Result<(), CodegenError> {
        let entry = self.ctx.layouts.entry(ty);
        let (position, _) = self.map_find(map, ty, key)?;
        let (found_block, missing_block, done) = (
            self.builder.create_block(),
            self.builder.create_block(),
            self.builder.create_block(),
        );
        let zero = self.iconst(types::I64, 0);
        let found = self
            .builder
            .ins()
            .icmp(IntCC::SignedGreaterThanOrEqual, position, zero);
        self.builder
            .ins()
            .brif(found, found_block, &[], missing_block, &[]);
        self.builder.switch_to_block(found_block);
        let address = self.entry_addr(map, position, &entry);
        let (entry_key, entry_value) = (
            self.offset(address, entry.key_offset),
            self.offset(address, entry.value_offset),
        );
        self.drop_value(entry_key, entry.key)?;
        self.store_some(option, option_ty, entry_value)?;
        let size = self.iconst(self.pointer, u64::from(entry.size));
        self.call(self.ctx.runtime.map_remove, &[map, position, size]);
        self.builder.ins().jump(done, &[]);
        self.builder.switch_to_block(missing_block);
        self.store_none(option);
        self.builder.ins().jump(done, &[]);
        self.builder.switch_to_block(done);
        Ok(())
    }

    /// Stores `[some value]` in the option at `option`, moving the value at `value`.
    pub(crate) fn store_some(
        &mut self,
        option: Value,
        option_ty: Ty,
        value: Value,
    ) -> Result<(), CodegenError> {
        let Ty::Option(inner) = option_ty else {
            return Err(CodegenError(format!("{option_ty} is not an option")));
        };
        let offset = self
            .ctx
            .layouts
            .variant_field_offset(option_ty, 1, 0)
            .ok_or_else(|| CodegenError(format!("no layout for {option_ty}")))?;
        let one = self.iconst(types::I32, 1);
        self.builder
            .ins()
            .store(MemFlagsData::trusted(), one, option, 0);
        let payload = self.offset(option, offset);
        self.copy_bytes(payload, value, *inner);
        Ok(())
    }

    /// Stores `none` in the option at `option`.
    pub(crate) fn store_none(&mut self, option: Value) {
        let zero = self.iconst(types::I32, 0);
        self.builder
            .ins()
            .store(MemFlagsData::trusted(), zero, option, 0);
    }

    // ----- Lists -------------------------------------------------------------------------

    /// Moves the last element of the list at `list` into the option at `option`, or stores
    /// `none` if the list is empty.
    pub(crate) fn list_pop(
        &mut self,
        list: Value,
        element: Ty,
        option: Value,
        option_ty: Ty,
    ) -> Result<(), CodegenError> {
        let len = self.len(list);
        let (some_block, none_block, done) = (
            self.builder.create_block(),
            self.builder.create_block(),
            self.builder.create_block(),
        );
        self.builder
            .ins()
            .brif(len, some_block, &[], none_block, &[]);
        self.builder.switch_to_block(some_block);
        let last_index = self.add_int(len, -1);
        self.set_len(list, last_index);
        let address = self.element_addr(list, last_index, element);
        self.store_some(option, option_ty, address)?;
        self.builder.ins().jump(done, &[]);
        self.builder.switch_to_block(none_block);
        self.store_none(option);
        self.builder.ins().jump(done, &[]);
        self.builder.switch_to_block(done);
        Ok(())
    }

    /// Whether the list at `list` has an element equal to `value`.
    pub(crate) fn list_contains(
        &mut self,
        list: Value,
        element: Ty,
        value: Loaded,
    ) -> Result<Value, CodegenError> {
        let len = self.len(list);
        self.search(len, |this, index| {
            let address = this.element_addr(list, index, element);
            let current = this.load_value(address, element);
            let ordering = this.compare(element, current, value)?;
            let equal = this.iconst(types::I8, EQUAL);
            Ok(this.builder.ins().icmp(IntCC::Equal, ordering, equal))
        })
    }

    /// Fills the new list at `dest` with copies of the elements of the lists at `sources`.
    pub(crate) fn list_concat(
        &mut self,
        dest: Value,
        ty: Ty,
        sources: &[Value],
    ) -> Result<(), CodegenError> {
        let Ty::List(element) = ty else {
            return Err(CodegenError(format!("{ty} is not a list")));
        };
        self.init_collection(dest, ty);
        let mut total = self.iconst(types::I64, 0);
        for &source in sources {
            let len = self.len(source);
            total = self.builder.ins().iadd(total, len);
        }
        self.list_reserve(dest, total, *element);
        for &source in sources {
            let len = self.len(source);
            self.for_each(len, |this, index| {
                let from = this.element_addr(source, index, *element);
                let to = this.list_push_slot(dest, *element);
                this.clone_value(to, from, *element)
            })?;
        }
        Ok(())
    }

    /// A new list at `dest` of copies of the keys (or values) of the map at `map`.
    pub(crate) fn map_parts(
        &mut self,
        dest: Value,
        map: Value,
        ty: Ty,
        keys: bool,
    ) -> Result<(), CodegenError> {
        let entry = self.ctx.layouts.entry(ty);
        let (part, offset) = if keys {
            (entry.key, entry.key_offset)
        } else {
            (entry.value, entry.value_offset)
        };
        self.init_collection(dest, Ty::list(part));
        let len = self.len(map);
        self.list_reserve(dest, len, part);
        self.for_each(len, |this, index| {
            let address = this.entry_addr(map, index, &entry);
            let from = this.offset(address, offset);
            let to = this.list_push_slot(dest, part);
            this.clone_value(to, from, part)
        })
    }

    /// Destroys every element of the collection at `collection`, which becomes empty.
    pub(crate) fn clear(&mut self, collection: Value, ty: Ty) -> Result<(), CodegenError> {
        self.drop_elements(collection, ty)?;
        match ty {
            Ty::List(_) => {
                let zero = self.iconst(types::I64, 0);
                self.set_len(collection, zero);
            }
            _ => {
                self.call(self.ctx.runtime.map_clear, &[collection]);
            }
        }
        Ok(())
    }

    /// Destroys the elements (or keys and values) of a collection, keeping its buffers.
    fn drop_elements(&mut self, collection: Value, ty: Ty) -> Result<(), CodegenError> {
        let len = self.len(collection);
        if let Ty::List(element) = ty {
            if self.ctx.layouts.needs_drop(*element) {
                self.for_each(len, |this, index| {
                    let address = this.element_addr(collection, index, *element);
                    this.drop_value(address, *element)
                })?;
            }
        } else {
            {
                let entry = self.ctx.layouts.entry(ty);
                if self.ctx.layouts.needs_drop(entry.key)
                    || self.ctx.layouts.needs_drop(entry.value)
                {
                    self.for_each(len, |this, index| {
                        let address = this.entry_addr(collection, index, &entry);
                        let key = this.offset(address, entry.key_offset);
                        this.drop_value(key, entry.key)?;
                        let value = this.offset(address, entry.value_offset);
                        this.drop_value(value, entry.value)
                    })?;
                }
            }
        }
        Ok(())
    }

    // ----- Glue --------------------------------------------------------------------------

    /// Emits the body of a glue function of a list, map or set type; returns its result.
    pub(crate) fn define_collection_glue(
        &mut self,
        ty: Ty,
        kind: GlueKind,
        params: &[Value],
    ) -> Result<Option<Value>, CodegenError> {
        match kind {
            GlueKind::Drop => {
                let collection = params[0];
                self.drop_elements(collection, ty)?;
                if let Ty::List(element) = ty {
                    let (size, align) = self.element_size(*element);
                    self.call(self.ctx.runtime.list_free, &[collection, size, align]);
                } else {
                    let entry = self.ctx.layouts.entry(ty);
                    let size = self.iconst(self.pointer, u64::from(entry.size));
                    let align = self.iconst(self.pointer, u64::from(entry.align));
                    self.call(self.ctx.runtime.map_free, &[collection, size, align]);
                }
                Ok(None)
            }
            GlueKind::Clone => {
                let (dest, source) = (params[0], params[1]);
                if let Ty::List(_) = ty {
                    self.list_concat(dest, ty, &[source])?;
                } else {
                    self.map_clone(dest, source, ty)?;
                }
                Ok(None)
            }
            GlueKind::Compare => {
                let (a, b) = (params[0], params[1]);
                let result = if let Ty::List(element) = ty {
                    self.list_compare(a, b, *element)?
                } else {
                    self.map_equal(a, b, ty)?
                };
                Ok(Some(result))
            }
            GlueKind::Display => {
                self.display_collection(params[0], params[1], ty)?;
                Ok(None)
            }
            GlueKind::KeyEq | GlueKind::Hash => {
                Err(CodegenError(format!("no {kind:?} glue for {ty}")))
            }
        }
    }

    /// A copy of the map at `source` into the new map at `dest`, keeping its order.
    fn map_clone(&mut self, dest: Value, source: Value, ty: Ty) -> Result<(), CodegenError> {
        let entry = self.ctx.layouts.entry(ty);
        self.init_collection(dest, ty);
        let len = self.len(source);
        self.for_each(len, |this, index| {
            let from = this.entry_addr(source, index, &entry);
            let hash = this
                .builder
                .ins()
                .load(types::I64, MemFlagsData::trusted(), from, 0);
            let size = this.iconst(this.pointer, u64::from(entry.size));
            let align = this.iconst(this.pointer, u64::from(entry.align));
            let to = this
                .call(this.ctx.runtime.map_push, &[dest, hash, size, align])
                .expect("returns the entry");
            for (offset, part) in [
                (entry.key_offset, entry.key),
                (entry.value_offset, entry.value),
            ] {
                let (to, from) = (this.offset(to, offset), this.offset(from, offset));
                this.clone_value(to, from, part)?;
            }
            Ok(())
        })
    }

    /// Lists compare element by element; a list that is a prefix of the other is smaller.
    fn list_compare(&mut self, a: Value, b: Value, element: Ty) -> Result<Value, CodegenError> {
        let (len_a, len_b) = (self.len(a), self.len(b));
        let shorter = self.builder.ins().icmp(IntCC::SignedLessThan, len_a, len_b);
        let common = self.builder.ins().select(shorter, len_a, len_b);
        let done = self.builder.create_block();
        let result = self.builder.append_block_param(done, types::I8);
        let header = self.builder.create_block();
        let index = self.builder.append_block_param(header, types::I64);
        let (body_block, next_block, by_length) = (
            self.builder.create_block(),
            self.builder.create_block(),
            self.builder.create_block(),
        );
        let zero = self.iconst(types::I64, 0);
        self.builder.ins().jump(header, &[zero.into()]);
        self.builder.switch_to_block(header);
        let more = self
            .builder
            .ins()
            .icmp(IntCC::SignedLessThan, index, common);
        self.builder
            .ins()
            .brif(more, body_block, &[], by_length, &[]);
        self.builder.switch_to_block(body_block);
        let (address_a, address_b) = (
            self.element_addr(a, index, element),
            self.element_addr(b, index, element),
        );
        let (value_a, value_b) = (
            self.load_value(address_a, element),
            self.load_value(address_b, element),
        );
        let ordering = self.compare(element, value_a, value_b)?;
        self.builder
            .ins()
            .brif(ordering, done, &[ordering.into()], next_block, &[]);
        self.builder.switch_to_block(next_block);
        let next = self.add_int(index, 1);
        self.builder.ins().jump(header, &[next.into()]);
        self.builder.switch_to_block(by_length);
        let longer = self
            .builder
            .ins()
            .icmp(IntCC::SignedGreaterThan, len_a, len_b);
        let (less, greater, equal) = (
            self.iconst(types::I8, LESS),
            self.iconst(types::I8, GREATER),
            self.iconst(types::I8, EQUAL),
        );
        let not_shorter = self.builder.ins().select(longer, greater, equal);
        let by_len = self.builder.ins().select(shorter, less, not_shorter);
        self.builder.ins().jump(done, &[by_len.into()]);
        self.builder.switch_to_block(done);
        Ok(result)
    }

    /// Maps and sets are equal when they have the same keys with equal values, in any
    /// order. They have no order, so unequal ones compare as `LESS`.
    fn map_equal(&mut self, a: Value, b: Value, ty: Ty) -> Result<Value, CodegenError> {
        let entry = self.ctx.layouts.entry(ty);
        let (len_a, len_b) = (self.len(a), self.len(b));
        let done = self.builder.create_block();
        let result = self.builder.append_block_param(done, types::I8);
        let same_len = self.builder.create_block();
        let differs = self.builder.ins().icmp(IntCC::NotEqual, len_a, len_b);
        let less = self.iconst(types::I8, LESS);
        self.builder
            .ins()
            .brif(differs, done, &[less.into()], same_len, &[]);
        self.builder.switch_to_block(same_len);
        let header = self.builder.create_block();
        let index = self.builder.append_block_param(header, types::I64);
        let (body_block, found_block, next_block, all_equal) = (
            self.builder.create_block(),
            self.builder.create_block(),
            self.builder.create_block(),
            self.builder.create_block(),
        );
        let zero = self.iconst(types::I64, 0);
        self.builder.ins().jump(header, &[zero.into()]);
        self.builder.switch_to_block(header);
        let more = self.builder.ins().icmp(IntCC::SignedLessThan, index, len_a);
        self.builder
            .ins()
            .brif(more, body_block, &[], all_equal, &[]);
        self.builder.switch_to_block(body_block);
        let entry_a = self.entry_addr(a, index, &entry);
        let key_a = self.offset(entry_a, entry.key_offset);
        let (position, _) = self.map_find(b, ty, key_a)?;
        let zero = self.iconst(types::I64, 0);
        let missing = self
            .builder
            .ins()
            .icmp(IntCC::SignedLessThan, position, zero);
        let less = self.iconst(types::I8, LESS);
        self.builder
            .ins()
            .brif(missing, done, &[less.into()], found_block, &[]);
        self.builder.switch_to_block(found_block);
        let entry_b = self.entry_addr(b, position, &entry);
        let (value_a, value_b) = (
            self.offset(entry_a, entry.value_offset),
            self.offset(entry_b, entry.value_offset),
        );
        let (value_a, value_b) = (
            self.load_value(value_a, entry.value),
            self.load_value(value_b, entry.value),
        );
        let ordering = self.compare(entry.value, value_a, value_b)?;
        self.builder
            .ins()
            .brif(ordering, done, &[ordering.into()], next_block, &[]);
        self.builder.switch_to_block(next_block);
        let next = self.add_int(index, 1);
        self.builder.ins().jump(header, &[next.into()]);
        self.builder.switch_to_block(all_equal);
        let equal = self.iconst(types::I8, EQUAL);
        self.builder.ins().jump(done, &[equal.into()]);
        self.builder.switch_to_block(done);
        Ok(result)
    }

    /// `{a; b}` for lists and sets, `{k=v; ...}` for maps, with literal elements.
    fn display_collection(
        &mut self,
        collection: Value,
        dest: Value,
        ty: Ty,
    ) -> Result<(), CodegenError> {
        self.push_text(dest, "{")?;
        let len = self.len(collection);
        let entry = match ty {
            Ty::List(_) => None,
            _ => Some(self.ctx.layouts.entry(ty)),
        };
        self.for_each(len, |this, index| {
            let (first, rest) = (this.builder.create_block(), this.builder.create_block());
            this.builder.ins().brif(index, rest, &[], first, &[]);
            this.builder.switch_to_block(rest);
            this.push_text(dest, "; ")?;
            this.builder.ins().jump(first, &[]);
            this.builder.switch_to_block(first);
            match (ty, entry) {
                (Ty::List(element), _) => {
                    let address = this.element_addr(collection, index, *element);
                    let value = this.load_value(address, *element);
                    this.display(*element, value, dest, true)?;
                }
                (_, Some(entry)) => {
                    let address = this.entry_addr(collection, index, &entry);
                    let key = this.offset(address, entry.key_offset);
                    let key = this.load_value(key, entry.key);
                    this.display(entry.key, key, dest, true)?;
                    if let Ty::Map(_) = ty {
                        this.push_text(dest, "=")?;
                        let value = this.offset(address, entry.value_offset);
                        let value = this.load_value(value, entry.value);
                        this.display(entry.value, value, dest, true)?;
                    }
                }
                _ => {}
            }
            Ok(())
        })?;
        self.push_text(dest, "}")
    }
}
