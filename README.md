# Pika

Pika is a general-purpose, statically typed, compiled programming language. Its syntax is
command-oriented (`:commands`, `$variables`, `[command substitution]`, `key=value` arguments,
`do={}` blocks), paired with the semantics of a modern systems language:
value types, ownership without a garbage collector, structs and traits, generics, and native code
generation.

```pika
:fn fib n:u64 -> u64 do={
    :if ($n < 2) do={ :return $n }
    :return ([:fib ($n - 1)] + [:fib ($n - 2)])
}

:for i from=1 to=10 do={ :put "fib($i) = $[:fib $i]" }
```

The language is specified in [`docs/spec/v0.md`](docs/spec/v0.md).

## Status

Early development. Milestones M0 to M5 are complete:

- `pika check` reports syntax, name, type and initialization errors, and `:match` statements
  that do not cover every value.
- `pika run` compiles programs to native code with Cranelift and runs them. Programs can use
  integers, floats, `bool`, `char`, `Duration`, `String`, structs and enums (with methods and
  derived `Copy`, `Clone`, `Eq`, `Ord`, `Hash` and `Display`), options (`T?`), boxes
  (`Box<T>`) for recursive types, lists, maps and sets with indexing and `:foreach`, `:match`
  with nested patterns and guards, functions, globals and all control flow, with ownership
  checking: moves, borrows (`read`/`mut`/`owned` parameters, `:match` bindings and
  `:foreach` variables) and automatic destruction of owned values.
- Generic functions, structs and enums, with bounds, inferred or explicit type arguments, and
  monomorphized code (M5).
- User-defined traits with required and default functions, associated functions and
  supertraits, implemented by structs and enums and used as bounds, with static dispatch (M5).
- Built-in traits implemented by types: operators (`Add`, `Sub`, `Mul`, `Div`, `Rem`, `Neg`,
  `Concat`), destructors (`Drop`), `Default` with `[:default]`, and `Display` with `fmt` (M5).
- Errors: `raises` functions, `:error`, `:onerror`, causes, and uncaught errors reported by
  `main` (M5). A call that can raise is marked with `?` after its head: `[/fs/read? $path]`.
- Function values: closures that capture by value, named functions as values, and function
  types `fn(A) -> R` (M5).

M6 is in progress:

- Packages with a `pika.toml` manifest and path dependencies, one module per source file,
  paths to the items of other modules (`/geo/shapes/area`), `:use` imports and aliases, and
  `_` privacy for items, fields and methods. Panic and error reports name the file.
- A first standard library, written in Pika in `std/`: math (float functions, integer
  arithmetic that does not trap, bits), random numbers, strings (searching, splitting,
  padding, number formatting and parsing), characters, collections (sorting, searching,
  `map`/`filter`/`fold` with closures, a heap and a deque), standard input and output,
  program arguments and environment variables, clocks, and files. Its few primitives are
  implemented once in the runtime, for both compiled and interpreted programs.
- Tests: `:test "name" do={...}` and `pika test`, which runs each test in a process of its
  own and reports failed assertions with the values compared.
- `pika fmt`, which rewrites files in the canonical layout (spec section 3.9), and
  `pika fmt --check` for CI.
- Methods of built-in types, which the standard library declares with `:impl`:
  `[$text->trim]`, `[[$line->split ","]->map $f]`, `[$items->sort]`, `[$x->sqrt]`,
  `[$n->checked_add 1]`, `[$option->expect "msg"]`.

Features of later milestones (foreign functions) are reported as "not supported yet". See the
milestones in section 17 of the spec.

## Building

Requires the Rust toolchain pinned in `rust-toolchain.toml` (installed automatically by
`rustup`).

```sh
cargo build
cargo test
cargo run -p pika_cli -- run path/to/file.pk     # compile and run a program
cargo run -p pika_cli -- run path/to/package     # run the package with a pika.toml there
cargo run -p pika_cli -- run app.pk -- a b       # pass arguments to the program
cargo run -p pika_cli -- test path/to/package    # run the tests of a package or file
cargo run -p pika_cli -- fmt path/to/package     # format the .pk files of a package
cargo run -p pika_cli -- run --interpret file.pk # run with the MIR interpreter instead
cargo run -p pika_cli -- check path/to/file.pk   # report errors and warnings
cargo run -p pika_cli -- types path/to/file.pk   # print inferred types
cargo run -p pika_cli -- parse path/to/file.pk   # print the syntax tree
cargo run -p pika_cli -- lex path/to/file.pk     # print the tokens
```

## Repository layout

| Path | Contents |
|---|---|
| `crates/pika_diagnostics` | source spans and diagnostic rendering |
| `crates/pika_syntax` | lexer, parser and typed AST over a lossless syntax tree |
| `crates/pika_hir` | lowering to a high-level IR with name resolution |
| `crates/pika_types` | type inference and checking |
| `crates/pika_mir` | control-flow graphs, definite assignment, constant evaluation, interpreter |
| `crates/pika_codegen` | native code generation with Cranelift |
| `crates/pika_runtime` | output, value formatting, panics and the standard library's primitives |
| `crates/pika_fmt` | the formatter: the canonical layout of source files |
| `crates/pika_driver` | loads packages and runs the compiler phases over their files |
| `crates/pika_cli` | the `pika` command-line tool |
| `std` | the standard library, written in Pika and embedded in the compiler |
| `editors/tree-sitter-pika` | a tree-sitter grammar, for editors |
| `editors/zed` | the [Zed](https://zed.dev) extension |
| `docs/spec` | the language specification |

## Development

Parser tests live in `crates/pika_syntax/tests/parser/`: files named `ok_*.pk` must parse
without diagnostics, and `err_*.pk` files must produce some. Semantic tests live in
`crates/pika_driver/tests/check/` with the same convention (`ok_*.pk` may have warnings but no
errors); their snapshots include the inferred types. Packages in
`crates/pika_driver/tests/check_packages/` are checked the same way, and the packages in
`crates/pika_driver/tests/load_errors/` must fail to load. Every `pika` code block in the spec
is also checked: plain blocks must parse cleanly, `pika syntax-error` blocks must fail, and
`pika fragment` blocks only need to lex.

Programs in `crates/pika_cli/tests/run/`, and packages in `crates/pika_cli/tests/run_packages/`,
are run both compiled and interpreted; the two runs must agree exactly, and the result is
snapshotted (`ok_*` must exit with 0, `panic_*` with 101, `error_*` with 1). The programs in
`crates/pika_cli/tests/test_runs/` are run with `pika test` the same way.
Compiled runs in tests set `PIKA_LEAK_CHECK=1`, which makes a program that does not free all of
its heap memory exit with status 102.
The differential property test (`crates/pika_cli/tests/differential.rs`) does the same for
random arithmetic programs over every numeric type.

The tree-sitter grammar must parse every program that the reference parser accepts; see
[`editors/tree-sitter-pika`](editors/tree-sitter-pika/README.md) for its tests and how to
regenerate it.

Property tests run 4096 random inputs per property by default; set `PROPTEST_CASES` for a deeper
run, for example `PROPTEST_CASES=1000000 cargo test --release --test lossless`.

Snapshot tests use [insta](https://insta.rs). After an intentional change to lexer output, run
`cargo insta review` (install with `cargo install cargo-insta`) or
`INSTA_UPDATE=always cargo test`, then review the changed `.snap` files before committing.
