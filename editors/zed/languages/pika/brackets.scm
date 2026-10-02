("(" @open
  ")" @close)

("[" @open
  "]" @close)

("{" @open
  "}" @close)

(type_arguments
  "<" @open
  ">" @close)

(type_parameters
  "<" @open
  ">" @close)

(interpolation
  "$(" @open
  ")" @close)

(interpolation
  "$[" @open
  "]" @close)

(("\"" @open
  "\"" @close)
  (#set! rainbow.exclude))
