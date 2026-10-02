; Functions and tests

(function_declaration
  body: (block
    "{"
    (_)* @function.inside
    "}")) @function.around

(function_declaration
  !body) @function.around

(test_declaration
  body: (block
    "{"
    (_)* @function.inside
    "}")) @function.around

; Types

(struct_declaration
  body: (struct_body
    "{"
    (_)* @class.inside
    "}")) @class.around

(enum_declaration
  body: (enum_body
    "{"
    (_)* @class.inside
    "}")) @class.around

(trait_declaration
  body: (function_list
    "{"
    (_)* @class.inside
    "}")) @class.around

(impl_declaration
  body: (function_list
    "{"
    (_)* @class.inside
    "}")) @class.around

; Comments

(comment)+ @comment.around

(doc_comment)+ @comment.around
