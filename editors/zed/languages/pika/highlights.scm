; Syntax highlighting. Later patterns take precedence over earlier ones.

; ----- Names ------------------------------------------------------------------------------------

(identifier) @variable

(variable) @variable

; All-caps names are constants, as in `:const MAX 100`.
((identifier) @constant
  (#match? @constant "^_*[A-Z][A-Z0-9_]*$"))

((variable) @constant
  (#match? @constant "^\\$_*[A-Z][A-Z0-9_]*$"))

((variable) @variable.special
  (#eq? @variable.special "$self"))

(self) @variable.special

(wildcard_pattern) @variable.special

; Module names in paths, such as `std` and `math` in `/std/math/sqrt`.
(path
  (identifier) @none)

; Types

(type_identifier) @type

(self_type) @type

((type_identifier) @type.builtin
  (#any-of? @type.builtin
    "i8" "i16" "i32" "i64" "u8" "u16" "u32" "u64" "int" "f32" "f64" "float" "bool" "char"
    "nothing" "never"))

(generic_type
  name: (path
    (identifier) @type .))

(typed_literal
  type: (path
    (identifier) @type .))

(type_parameter
  name: (identifier) @type)

(type_parameter
  bound: (type_identifier) @type.interface)

(trait_list
  (type_identifier) @type.interface)

(trait_declaration
  name: (identifier) @type.interface)

(struct_declaration
  name: (identifier) @type)

(enum_declaration
  name: (identifier) @type)

; Types and items of other modules, such as `/geo/Point` and `/math/pi`.
((path
  (identifier) @type .)
  (#match? @type "^_*[A-Z]"))

((path
  (identifier) @constant .)
  (#match? @constant "^_*[A-Z][A-Z0-9_]*$"))

; Fields and variants

(field_declaration
  name: (identifier) @property)

(field_initializer
  name: (identifier) @property)

(member_expression
  member: (identifier) @property)

(variant_declaration
  name: (identifier) @variant)

(member_expression
  object: [
    (type_identifier)
    (self_type)
    (generic_type)
    (path)
  ]
  member: (identifier) @variant)

(variant_pattern
  variant: (identifier) @variant)

; Functions

(function_declaration
  name: (identifier) @function.definition)

(parameter
  name: (identifier) @variable.parameter)

(argument_name) @label

(call
  head: (command_name) @function)

(call
  head: (path
    (identifier) @function .))

(call
  head: (member_expression
    member: (identifier) @function.method))

(call
  head: (variable) @function)

; Associated functions and variants with fields, called through their type, look the same:
; `[Point->new 1.0 2.0]`, `[Shape->circle 2.0]`. Both are highlighted as functions.
(call
  head: (member_expression
    object: [
      (type_identifier)
      (self_type)
      (generic_type)
      (path)
    ]
    member: (identifier) @function))

; Built-in commands (section 8.4).
((command_name) @function.builtin
  (#any-of? @function.builtin
    ":put" ":len" ":tostr" ":typeof" ":assert" ":nothing" ":default"))

((command_name) @keyword.control
  (#any-of? @keyword.control ":return" ":break" ":continue" ":error" ":panic"))

; ----- Keywords ---------------------------------------------------------------------------------

[
  ":local"
  ":const"
  ":global"
  ":set"
  ":fn"
  ":struct"
  ":enum"
  ":trait"
  ":impl"
  ":use"
  ":extern"
  ":test"
  ":unsafe"
  "fn"
  "impl"
  "lib"
  "raises"
  "as"
  "and"
  "or"
  "in"
  "some"
  (some)
  (convention)
  "mut"
] @keyword

[
  ":if"
  ":while"
  ":do"
  ":for"
  ":foreach"
  ":match"
  ":onerror"
  "do"
  "else"
  "from"
  "to"
  "until"
  "step"
  "while"
  "if"
] @keyword.control

; `in=` of `:foreach` and `:onerror`; elsewhere `in` is the membership operator.
(foreach_statement
  "in" @keyword.control)

(onerror_statement
  "in" @keyword.control)

; ----- Literals ---------------------------------------------------------------------------------

[
  (string)
  (raw_string)
  (char)
] @string

(string_content) @string

(escape_sequence) @string.escape

[
  (integer)
  (float)
  (duration)
] @number

(boolean) @boolean

(none) @constant.builtin

; ----- Punctuation and operators ----------------------------------------------------------------

(interpolation
  [
    "$("
    ")"
    "$["
    "]"
  ] @punctuation.special)

[
  "("
  ")"
  "["
  "]"
  "{"
  "}"
] @punctuation.bracket

(type_arguments
  [
    "<"
    ">"
  ] @punctuation.bracket)

(type_parameters
  [
    "<"
    ">"
  ] @punctuation.bracket)

[
  ";"
  ","
  ":"
] @punctuation.delimiter

[
  "->"
  "?"
] @punctuation.special

; The `?` after the head of a call that can raise an error.
(raise_mark) @keyword.control

(binary_expression
  operator: _ @operator)

(unary_expression
  operator: _ @operator)

(binary_expression
  operator: [
    "and"
    "or"
    "in"
  ] @keyword)

(type_parameter
  "+" @operator)

[
  "="
] @operator

; ----- Comments ---------------------------------------------------------------------------------

(comment) @comment

(doc_comment) @comment.doc
