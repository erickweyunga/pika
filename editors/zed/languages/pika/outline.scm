(doc_comment) @annotation

(function_declaration
  ":fn" @context
  name: (_) @name) @item

(struct_declaration
  ":struct" @context
  name: (_) @name) @item

(enum_declaration
  ":enum" @context
  name: (_) @name) @item

(trait_declaration
  ":trait" @context
  name: (_) @name) @item

(impl_declaration
  ":impl" @context
  type: (_) @name) @item

(extern_declaration
  ":extern" @context
  library: (_) @name) @item

(struct_body
  (field_declaration
    name: (_) @name) @item)

(variant_declaration
  name: (_) @name) @item

(test_declaration
  ":test" @context
  name: (_) @name) @item

; Module-level constants and globals.
(source_file
  (variable_declaration
    kind: _ @context
    name: (_) @name) @item)
