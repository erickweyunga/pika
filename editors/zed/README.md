<p align="center"><img src="../../docs/assets/pika-logo.png" alt="Pika" width="160"></p>

# Pika for Zed

Pika language support for the [Zed](https://zed.dev) editor, for `.pk` files:

- syntax highlighting, including interpolations inside strings
- the outline panel and breadcrumbs: functions, structs and their fields, enums and their
  variants, traits, tests, and module-level constants and globals
- bracket matching, auto-closing and auto-indentation
- comment toggling with `#`
- text objects for functions, types and comments (vim mode)
- a run button next to `:fn main`, and tasks to run or check the current file or package

The grammar is the tree-sitter grammar in [`../tree-sitter-pika`](../tree-sitter-pika).

## Requirements

The run button and the tasks call the `pika` command, which must be on your `PATH`:

```sh
cargo install --path crates/pika_cli
```

## Installing as a dev extension

Zed fetches the grammar with git from the repository and revision in `extension.toml`
(`[grammars.pika]`), and compiles `src/parser.c` and `src/scanner.c` from the directory named by
`path`. The revision must be a commit that contains the grammar.

1. Set `repository` and `rev` in `[grammars.pika]`. To try changes before they are pushed, use the
   local clone and a local commit:

   ```toml
   [grammars.pika]
   repository = "file:///path/to/pika"
   rev = "<commit SHA>"
   path = "editors/tree-sitter-pika"
   ```

2. In Zed, run `zed: install dev extension` from the command palette and select this directory,
   `editors/zed`.

After changing the grammar, commit it, update `rev`, and click `Rebuild` next to the extension
on the Extensions page. Changes to the files in `languages/pika` only need the rebuild. If
something does not work, `zed: open log` shows what went wrong while loading the extension.

## Files

| File | Purpose |
|---|---|
| `extension.toml` | the extension's manifest, and where the grammar comes from |
| `languages/pika/config.toml` | file suffix, comments, brackets and indentation settings |
| `languages/pika/highlights.scm` | syntax highlighting |
| `languages/pika/brackets.scm` | bracket pairs |
| `languages/pika/indents.scm` | auto-indentation |
| `languages/pika/outline.scm` | the outline panel and breadcrumbs |
| `languages/pika/textobjects.scm` | text objects and motions in vim mode |
| `languages/pika/overrides.scm` | strings and comments, where brackets do not auto-close |
| `languages/pika/runnables.scm`, `tasks.json` | the run button and tasks |

The queries are checked against the grammar by `cargo test -p tree-sitter-pika`, and the
highlighting by the tests in `../tree-sitter-pika/test/highlight` (see its README).
