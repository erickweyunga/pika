; `:fn main` in a file runs it as a program.
((source_file
  (function_declaration
    name: (identifier) @run))
  (#eq? @run "main")
  (#set! tag pika-main))
