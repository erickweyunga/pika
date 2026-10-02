// The external scanner of the Pika grammar.
//
// It lexes the tokens that depend on context the grammar cannot express:
//
// - NEWLINE: a line break that ends a statement. It is only produced where the grammar accepts
//   one (inside blocks, bodies, collection literals and at the top level); everywhere else, such
//   as inside `( )`, `[ ]` and `< >`, line breaks are whitespace (section 3.4).
// - ARGUMENT_NAME: the `name` of `name=value`, recognized by the `=` joined after it.
// - STRING_CONTENT: the literal text of a string, up to an escape, an interpolation or the
//   closing quote. Spaces and `#` inside a string are text, not whitespace or a comment.
// - RAW_STRING: `r"..."` and `r#"..."#`, which end at a quote followed by as many `#` as they
//   started with.
//
// The scanner keeps no state between tokens.

#include "tree_sitter/parser.h"

#include <stdbool.h>
#include <stdint.h>

enum TokenType {
  NEWLINE,
  ARGUMENT_NAME,
  STRING_CONTENT,
  RAW_STRING,
  ERROR_SENTINEL,
};

void *tree_sitter_pika_external_scanner_create(void) { return NULL; }

void tree_sitter_pika_external_scanner_destroy(void *payload) { (void)payload; }

unsigned tree_sitter_pika_external_scanner_serialize(void *payload, char *buffer) {
  (void)payload;
  (void)buffer;
  return 0;
}

void tree_sitter_pika_external_scanner_deserialize(void *payload, const char *buffer,
                                                   unsigned length) {
  (void)payload;
  (void)buffer;
  (void)length;
}

static bool is_ident_start(int32_t c) {
  return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || c == '_';
}

static bool is_ident_continue(int32_t c) { return is_ident_start(c) || (c >= '0' && c <= '9'); }

static bool is_line_break(int32_t c) { return c == '\n' || c == '\r'; }

static void advance(TSLexer *lexer) { lexer->advance(lexer, false); }

static void skip(TSLexer *lexer) { lexer->advance(lexer, true); }

// Consumes a line break: `\n`, `\r\n` or `\r`.
static void advance_line_break(TSLexer *lexer, bool skipping) {
  bool carriage_return = lexer->lookahead == '\r';
  lexer->advance(lexer, skipping);
  if (carriage_return && lexer->lookahead == '\n') {
    lexer->advance(lexer, skipping);
  }
}

// The text of a string up to `"`, `\` or an interpolation (`$name`, `$(` or `$[`). A `$` that
// starts none of these is text; the reference lexer reports it.
static bool scan_string_content(TSLexer *lexer) {
  bool has_content = false;
  for (;;) {
    if (lexer->eof(lexer)) {
      break;
    }
    int32_t c = lexer->lookahead;
    if (c == '"' || c == '\\') {
      break;
    }
    if (c == '$') {
      lexer->mark_end(lexer);
      advance(lexer);
      int32_t next = lexer->lookahead;
      if (is_ident_start(next) || next == '(' || next == '[') {
        // The content ends before the `$`, where `mark_end` was called.
        if (has_content) {
          lexer->result_symbol = STRING_CONTENT;
        }
        return has_content;
      }
      has_content = true;
      continue;
    }
    advance(lexer);
    has_content = true;
  }
  lexer->mark_end(lexer);
  lexer->result_symbol = STRING_CONTENT;
  return has_content;
}

// The rest of a raw string, after its `r`. Returns false if no `"` follows the `#` signs.
static bool scan_raw_string(TSLexer *lexer) {
  unsigned hashes = 0;
  while (lexer->lookahead == '#') {
    advance(lexer);
    hashes++;
  }
  if (lexer->lookahead != '"') {
    return false;
  }
  advance(lexer);
  for (;;) {
    if (lexer->eof(lexer)) {
      // Unterminated; the reference lexer reports it.
      break;
    }
    if (lexer->lookahead == '"') {
      advance(lexer);
      unsigned closing = 0;
      while (closing < hashes && lexer->lookahead == '#') {
        advance(lexer);
        closing++;
      }
      if (closing == hashes) {
        break;
      }
      continue;
    }
    advance(lexer);
  }
  lexer->mark_end(lexer);
  lexer->result_symbol = RAW_STRING;
  return true;
}

// A name directly followed by `=` (but not `==`), as in `from=1`. The `=` is not part of the
// token. Called with the first character of the name already consumed.
static bool scan_argument_name(TSLexer *lexer) {
  while (is_ident_continue(lexer->lookahead)) {
    advance(lexer);
  }
  if (lexer->lookahead != '=') {
    return false;
  }
  lexer->mark_end(lexer);
  advance(lexer);
  if (lexer->lookahead == '=') {
    return false;
  }
  lexer->result_symbol = ARGUMENT_NAME;
  return true;
}

bool tree_sitter_pika_external_scanner_scan(void *payload, TSLexer *lexer,
                                            const bool *valid_symbols) {
  (void)payload;

  // While recovering from an error every token is valid. Only line breaks are produced then,
  // which helps the parser find the end of the broken statement.
  bool recovering = valid_symbols[ERROR_SENTINEL];

  // Inside a string, whitespace is text, so nothing is skipped.
  if (valid_symbols[STRING_CONTENT] && !recovering) {
    return scan_string_content(lexer);
  }

  // Skip spaces, tabs and line continuations (a backslash at the end of a line), and line
  // breaks where they do not end a statement.
  bool newline = valid_symbols[NEWLINE];
  for (;;) {
    int32_t c = lexer->lookahead;
    if (c == ' ' || c == '\t' || c == '\f' || c == 0xFEFF || (!newline && is_line_break(c))) {
      skip(lexer);
    } else if (c == '\\') {
      skip(lexer);
      if (!is_line_break(lexer->lookahead)) {
        return false;
      }
      advance_line_break(lexer, true);
    } else {
      break;
    }
  }

  if (is_line_break(lexer->lookahead)) {
    advance_line_break(lexer, false);
    lexer->mark_end(lexer);
    lexer->result_symbol = NEWLINE;
    return true;
  }

  if (recovering) {
    return false;
  }

  bool raw_string = valid_symbols[RAW_STRING];
  bool argument_name = valid_symbols[ARGUMENT_NAME];
  if (!is_ident_start(lexer->lookahead) || !(raw_string || argument_name)) {
    return false;
  }
  if (lexer->lookahead == 'r') {
    advance(lexer);
    if (lexer->lookahead == '"' || lexer->lookahead == '#') {
      return raw_string && scan_raw_string(lexer);
    }
  } else {
    advance(lexer);
  }
  return argument_name && scan_argument_name(lexer);
}
