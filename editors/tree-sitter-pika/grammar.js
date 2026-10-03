/**
 * @file Tree-sitter grammar for Pika
 *
 * Follows the reference parser in `crates/pika_syntax` (section 16 of the specification in
 * `docs/spec/v0.md`). Where the reference parser uses the "joined" flag of a token (no
 * whitespace before it), this grammar uses `token.immediate`. The external scanner in
 * `src/scanner.c` decides which line breaks end a statement, recognizes `name=` at the start of
 * a named argument, and lexes the text of strings and raw strings.
 */

/// <reference types="tree-sitter-cli/dsl" />
// @ts-check

// Binding powers of the operators, from section 7.1 of the specification.
const PREC = {
  or: 1,
  and: 3,
  comparison: 5,
  concat: 7,
  bit_or: 9,
  bit_xor: 11,
  bit_and: 13,
  shift: 15,
  additive: 17,
  multiplicative: 19,
  cast: 21,
  unary: 23,
};

const IDENTIFIER = /[A-Za-z_][A-Za-z0-9_]*/;

const imm = token.immediate;

module.exports = grammar({
  name: 'pika',

  word: $ => $.identifier,

  externals: $ => [
    // A line break that ends a statement or an element. The scanner only produces it where the
    // grammar accepts one, so line breaks inside `( )`, `[ ]` and `< >` are whitespace.
    $._newline,
    // The `name` of `name=value`, which the scanner recognizes by looking at the `=` after it.
    $._argument_name,
    $.string_content,
    $.raw_string,
    // Never used by the grammar: it is only valid while the parser recovers from an error.
    $._error_sentinel,
  ],

  extras: $ => [
    /[ \t\f\r\n﻿]/,
    $.comment,
    $.doc_comment,
    // A backslash at the end of a line joins it with the next one.
    /\\\r?\n/,
  ],

  supertypes: $ => [$._statement, $._expression, $._pattern, $._type],

  rules: {
    source_file: $ => statements($, $._statement),

    // ----- Statements --------------------------------------------------------------------------

    _statement: $ => choice($.block, $._command),

    _command: $ => choice($._form, $.call),

    // The built-in forms, whose arguments are parsed specially (section 8).
    _form: $ => choice(
      $.variable_declaration,
      $.set_statement,
      $.if_statement,
      $.while_statement,
      $.do_while_statement,
      $.for_statement,
      $.foreach_statement,
      $.match_statement,
      $.onerror_statement,
      $.unsafe_block,
      $.function_declaration,
      $.struct_declaration,
      $.enum_declaration,
      $.trait_declaration,
      $.impl_declaration,
      $.use_declaration,
      $.extern_declaration,
      $.test_declaration,
    ),

    block: $ => seq('{', statements($, $._statement), '}'),

    variable_declaration: $ => seq(
      field('kind', choice(':local', ':const', ':global')),
      field('name', $.identifier),
      optional(seq(imm(':'), field('type', $._type))),
      optional(field('value', $._atom)),
    ),

    set_statement: $ => seq(
      ':set',
      field('target', choice($.identifier, $.parenthesized_expression)),
      field('value', $._atom),
    ),

    if_statement: $ => prec.right(seq(
      ':if',
      field('condition', $.parenthesized_expression),
      'do', imm('='), field('body', $.block),
      optional(seq('else', imm('='), field('alternative', choice($.block, $.if_statement)))),
    )),

    while_statement: $ => seq(
      ':while',
      field('condition', $.parenthesized_expression),
      'do', imm('='), field('body', $.block),
    ),

    do_while_statement: $ => seq(
      ':do',
      field('body', $.block),
      'while', imm('='), field('condition', $.parenthesized_expression),
    ),

    for_statement: $ => seq(
      ':for',
      field('variable', $.identifier),
      repeat1(choice(
        seq('from', imm('='), field('start', $._atom)),
        seq('to', imm('='), field('end', $._atom)),
        seq('until', imm('='), field('end', $._atom)),
        seq('step', imm('='), field('step', $._atom)),
      )),
      'do', imm('='), field('body', $.block),
    ),

    foreach_statement: $ => seq(
      ':foreach',
      optional('mut'),
      field('variable', $.identifier),
      optional(seq(',', field('variable', $.identifier))),
      'in', imm('='), field('collection', $._atom),
      'do', imm('='), field('body', $.block),
    ),

    match_statement: $ => seq(
      ':match',
      field('value', $._atom),
      field('body', $.match_body),
    ),

    match_body: $ => seq('{', statements($, $.match_arm), '}'),

    match_arm: $ => seq(
      field('pattern', $._pattern),
      optional(seq('if', imm('='), field('guard', $.parenthesized_expression))),
      'do', imm('='), field('body', $.block),
    ),

    onerror_statement: $ => seq(
      ':onerror',
      field('name', $.identifier),
      'in', imm('='), field('body', $.block),
      'do', imm('='), field('handler', $.block),
    ),

    unsafe_block: $ => seq(':unsafe', field('body', $.block)),

    // ----- Calls -------------------------------------------------------------------------------

    // A command that is not a built-in form: `:name args`, `/path args`, `$value args`,
    // `Type->member args`, or `[command]->member args`.
    call: $ => callRule($, $._head_atom),

    // Inside `[ ]` and `$[ ]`, any atom can be the head, including a collection literal; at the
    // start of a statement, `{` always starts a block.
    _bracketed_call: $ => callRule($, $._atom),

    _bracketed_command: $ => choice($._form, alias($._bracketed_call, $.call)),

    named_argument: $ => seq(
      field('name', alias($._argument_name, $.argument_name)),
      imm('='),
      field('value', $._atom),
    ),

    // ----- Declarations ------------------------------------------------------------------------

    // `:fn name<T> params -> Type raises do={...}`. The name is absent for anonymous functions
    // (`[:fn x:i64 -> i64 do={...}]`), and the body for signatures in traits and `:extern`.
    function_declaration: $ => prec.right(seq(
      ':fn',
      optional(seq(
        field('name', $.identifier),
        optional(field('type_parameters', $.type_parameters)),
      )),
      repeat(field('parameter', $.parameter)),
      optional(seq('->', field('return_type', $._type))),
      optional('raises'),
      optional(seq('do', imm('='), field('body', $.block))),
    )),

    parameter: $ => choice(
      seq(optional(field('convention', $.convention)), $.self),
      seq(
        optional(field('convention', $.convention)),
        field('name', $.identifier),
        imm(':'),
        field('type', $._type),
        optional(seq(imm('='), field('default', $._atom))),
      ),
    ),

    convention: _ => choice('mut', 'owned'),

    self: _ => 'self',

    struct_declaration: $ => seq(
      ':struct',
      typeDeclarationHeader($),
      field('body', $.struct_body),
    ),

    struct_body: $ => seq(
      '{',
      statements($, choice($.field_declaration, $.function_declaration)),
      '}',
    ),

    field_declaration: $ => seq(
      field('name', $.identifier),
      imm(':'),
      field('type', $._type),
      optional(seq(imm('='), field('default', $._atom))),
    ),

    enum_declaration: $ => seq(
      ':enum',
      typeDeclarationHeader($),
      field('body', $.enum_body),
    ),

    enum_body: $ => seq(
      '{',
      statements($, choice($.variant_declaration, $.function_declaration)),
      '}',
    ),

    variant_declaration: $ => seq(
      field('name', $.identifier),
      repeat(field('field', $.field_declaration)),
    ),

    trait_declaration: $ => seq(
      ':trait',
      typeDeclarationHeader($),
      field('body', $.function_list),
    ),

    // `:impl<T> Type impl=Trait { ... }`, reserved for trait extensions.
    impl_declaration: $ => seq(
      ':impl',
      optional(field('type_parameters', $.type_parameters)),
      field('type', $._type),
      optional(seq('impl', imm('='), field('traits', $.trait_list))),
      field('body', $.function_list),
    ),

    function_list: $ => seq('{', statements($, $.function_declaration), '}'),

    trait_list: $ => seq($._type, repeat(seq(',', $._type))),

    use_declaration: $ => seq(
      ':use',
      field('path', $.path),
      optional(seq('as', imm('='), field('alias', $.identifier))),
    ),

    extern_declaration: $ => seq(
      ':extern',
      'lib', imm('='), field('library', $._atom),
      field('body', $.function_list),
    ),

    test_declaration: $ => seq(
      ':test',
      field('name', $.string),
      'do', imm('='), field('body', $.block),
    ),

    // ----- Atoms and expressions ---------------------------------------------------------------

    // An atom: a command argument, or an operand inside an expression (section 4.1).
    _atom: $ => choice($._primary, $.collection_literal, $.member_expression),

    // The atoms that can start a statement: every atom except a collection literal, which
    // would be a block there.
    _head_atom: $ => choice($._primary, alias($._head_member_expression, $.member_expression)),

    _primary: $ => choice(
      $._literal,
      $.string,
      $.variable,
      $.path,
      // `/std/num/parse<i64>`, `Tree<i64>->leaf`
      $.generic_type,
      $.parenthesized_expression,
      $.command_substitution,
      $.typed_literal,
    ),

    // A type name used as a value, which must be followed by `->member` or `{...}`.
    _type_head: $ => choice(alias($.identifier, $.type_identifier), $.self_type),

    member_expression: $ => memberRule($, choice($._atom, $._type_head)),

    _head_member_expression: $ => memberRule($, choice($._head_atom, $._type_head)),

    command_substitution: $ => seq('[', $._bracketed_command, ']'),

    parenthesized_expression: $ => seq(
      '(',
      $._expression,
      // Tuples are reserved; the reference parser reports them.
      repeat(seq(',', $._expression)),
      ')',
    ),

    _expression: $ => choice(
      $._atom,
      $.unary_expression,
      $.binary_expression,
      $.cast_expression,
    ),

    unary_expression: $ => prec(PREC.unary, seq(
      field('operator', choice('-', '!', '~')),
      field('operand', $._expression),
    )),

    binary_expression: $ => {
      const table = [
        [PREC.or, choice('or', '||')],
        [PREC.and, choice('and', '&&')],
        [PREC.comparison, choice('=', '!=', '<', '<=', '>', '>=', 'in')],
        [PREC.concat, '.'],
        [PREC.bit_or, '|'],
        [PREC.bit_xor, '^'],
        [PREC.bit_and, '&'],
        [PREC.shift, choice('<<', '>>')],
        [PREC.additive, choice('+', '-')],
        [PREC.multiplicative, choice('*', '/', '%')],
      ];
      return choice(...table.map(([precedence, operator]) => prec.left(precedence, seq(
        field('left', $._expression),
        field('operator', operator),
        field('right', $._expression),
      ))));
    },

    cast_expression: $ => prec.left(PREC.cast, seq(
      field('value', $._expression),
      'as',
      field('type', $._type),
    )),

    // `{1; 2}`, `{"a"=1}`
    collection_literal: $ => seq('{', statements($, $._element), '}'),

    // `Point{x=1.0; y=2.0}`, `List<i64>{}`, `/geo/Point{...}`
    typed_literal: $ => seq(
      field('type', choice($._type_head, $.path, $.generic_type)),
      imm('{'),
      statements($, $._element),
      '}',
    ),

    _element: $ => choice($._atom, $.map_entry, $.field_initializer),

    map_entry: $ => seq(field('key', $._atom), '=', field('value', $._atom)),

    field_initializer: $ => seq(
      field('name', alias($._argument_name, $.identifier)),
      imm('='),
      field('value', $._atom),
    ),

    // `/std/math/sqrt`: every `/` and name are joined.
    path: $ => seq(
      '/',
      alias(imm(IDENTIFIER), $.identifier),
      repeat(seq(imm('/'), alias(imm(IDENTIFIER), $.identifier))),
    ),

    variable: _ => /\$[A-Za-z_][A-Za-z0-9_]*/,

    // ----- Literals ----------------------------------------------------------------------------

    _literal: $ => choice(
      $.integer,
      $.float,
      $.duration,
      $.char,
      $.raw_string,
      $.boolean,
      $.none,
    ),

    // A `-` joined to a number is part of it: `:put -5` (section 3.3).
    integer: _ => token(seq(optional('-'), choice(
      /[0-9][0-9_]*/,
      /0x[0-9A-Fa-f_]+/,
      /0o[0-7_]+/,
      /0b[01_]+/,
    ))),

    float: _ => token(seq(optional('-'), choice(
      /[0-9][0-9_]*\.[0-9][0-9_]*([eE][+-]?[0-9][0-9_]*)?/,
      /[0-9][0-9_]*[eE][+-]?[0-9][0-9_]*/,
    ))),

    // `500ms`, `1m30s`, `1d12h`. Like the reference lexer, everything after the first unit that
    // could continue a name belongs to the literal.
    duration: _ => token(seq(optional('-'), /[0-9][0-9_]*(ns|us|ms|[wdhms])[0-9A-Za-z_]*/)),

    char: _ => token(seq(
      '\'',
      choice(
        /[^'\\\r\n]/,
        /\\u\{[0-9A-Fa-f]*\}/,
        /\\x[0-9A-Fa-f]{2}/,
        /\\[^\r\n]/,
      ),
      '\'',
    )),

    boolean: _ => choice('true', 'false'),

    none: _ => 'none',

    string: $ => seq(
      '"',
      repeat(choice($.string_content, $.escape_sequence, $.interpolation)),
      '"',
    ),

    escape_sequence: _ => imm(prec(1, seq(
      '\\',
      choice(
        /u\{[0-9A-Fa-f]*\}/,
        /x[0-9A-Fa-f]{2}/,
        // A line continuation: the line break and the indentation after it are removed.
        /\r?\n[ \t]*/,
        /[^\r\n]/,
      ),
    ))),

    // `$name`, `$(expression)` or `$[command]` inside a string.
    interpolation: $ => choice(
      $.variable,
      seq('$(', $._expression, ')'),
      seq('$[', $._bracketed_command, ']'),
    ),

    // ----- Types -------------------------------------------------------------------------------

    _type: $ => choice(
      alias($.identifier, $.type_identifier),
      $.self_type,
      $.path,
      $.generic_type,
      $.option_type,
      $.function_type,
    ),

    self_type: _ => 'Self',

    generic_type: $ => seq(
      field('name', choice(alias($.identifier, $.type_identifier), $.self_type, $.path)),
      field('type_arguments', $.type_arguments),
    ),

    // A `<` joined to a name always starts type arguments (section 3.3).
    type_arguments: $ => seq(
      imm(prec(1, '<')),
      $._type,
      repeat(seq(',', $._type)),
      '>',
    ),

    option_type: $ => seq($._type, imm('?')),

    // `fn(A, B) -> R raises`. A `->` or `raises` after a function type belongs to it.
    function_type: $ => prec.right(seq(
      'fn',
      '(',
      optional(seq($._type, repeat(seq(',', $._type)))),
      ')',
      optional(seq('->', field('return_type', $._type))),
      optional('raises'),
    )),

    type_parameters: $ => seq(
      imm(prec(1, '<')),
      $.type_parameter,
      repeat(seq(',', $.type_parameter)),
      '>',
    ),

    type_parameter: $ => seq(
      field('name', $.identifier),
      optional(seq(':', field('bound', $._type), repeat(seq('+', field('bound', $._type))))),
    ),

    // ----- Patterns ----------------------------------------------------------------------------

    _pattern: $ => choice($._simple_pattern, $.variant_pattern),

    // A pattern inside another one, where a variant pattern cannot have fields unless it is
    // parenthesized: `Shape->pair (some x) Color->red`.
    _simple_pattern: $ => choice(
      $.wildcard_pattern,
      $.binding_pattern,
      $._literal_pattern,
      $.some_pattern,
      $.parenthesized_pattern,
      alias($._unit_variant_pattern, $.variant_pattern),
    ),

    wildcard_pattern: _ => '_',

    binding_pattern: $ => $.identifier,

    _literal_pattern: $ => choice($._literal, $.string),

    some_pattern: $ => seq('some', field('pattern', $._simple_pattern)),

    parenthesized_pattern: $ => seq('(', $._pattern, ')'),

    variant_pattern: $ => prec.right(seq(
      variantHead($),
      repeat1(field('field', $._simple_pattern)),
    )),

    _unit_variant_pattern: $ => variantHead($),

    // ----- Names and comments ------------------------------------------------------------------

    identifier: _ => IDENTIFIER,

    command_name: _ => /:[A-Za-z_][A-Za-z0-9_]*/,

    comment: _ => token(prec(-1, /#[^\r\n]*/)),

    doc_comment: _ => token(seq('##', /[^\r\n]*/)),
  },
});

/**
 * Items separated by `;` or line breaks, with any number of empty items: what a file, a block,
 * a body or a collection literal contains.
 *
 * @param {GrammarSymbols<string>} $
 * @param {RuleOrLiteral} rule
 * @returns {RuleOrLiteral}
 */
function statements($, rule) {
  const terminator = choice(';', $._newline);
  return seq(optional(rule), repeat(seq(terminator, optional(rule))));
}

/**
 * A call with the given head, and its arguments.
 *
 * @param {GrammarSymbols<string>} $
 * @param {RuleOrLiteral} head
 * @returns {RuleOrLiteral}
 */
function callRule($, head) {
  return prec.right(seq(
    choice(
      seq(
        field('head', $.command_name),
        optional(field('type_arguments', $.type_arguments)),
      ),
      // `[some $x]`
      field('head', alias('some', $.some)),
      field('head', head),
    ),
    // `?` joined to the head marks a call that can raise an error: `[/fs/read? $path]`.
    optional(field('raise_mark', alias(token.immediate('?'), $.raise_mark))),
    repeat(field('argument', choice($._atom, $.named_argument))),
  ));
}

/**
 * `object->member`, where the `->` is joined to the object. A spaced `->` is a return type.
 *
 * @param {GrammarSymbols<string>} $
 * @param {RuleOrLiteral} object
 * @returns {RuleOrLiteral}
 */
function memberRule($, object) {
  return prec.left(seq(
    field('object', object),
    imm('->'),
    field('member', choice(
      seq($.identifier, optional(field('type_arguments', $.type_arguments))),
      $.integer,
      $.char,
      $.raw_string,
      $.string,
      $.variable,
      $.parenthesized_expression,
    )),
  ));
}

/**
 * The name, type parameters and `impl=` list of a struct, enum or trait.
 *
 * @param {GrammarSymbols<string>} $
 * @returns {RuleOrLiteral}
 */
function typeDeclarationHeader($) {
  return seq(
    field('name', $.identifier),
    optional(field('type_parameters', $.type_parameters)),
    optional(seq('impl', imm('='), field('traits', $.trait_list))),
  );
}

/**
 * `Type->variant`, with the type optionally a path or generic.
 *
 * @param {GrammarSymbols<string>} $
 * @returns {RuleOrLiteral}
 */
function variantHead($) {
  return seq(
    field('type', choice($._type_head, $.path, $.generic_type)),
    imm('->'),
    field('variant', $.identifier),
  );
}
