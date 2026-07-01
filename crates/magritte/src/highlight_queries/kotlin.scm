[
  (line_comment)
  (multiline_comment)
  (shebang_line)
] @comment

[
  (real_literal)
  (integer_literal)
  (long_literal)
  (hex_literal)
  (bin_literal)
  (unsigned_literal)
] @number

[
  (null_literal)
  (boolean_literal)
] @boolean

[
  (character_literal)
  (string_literal)
] @string

(character_escape_seq) @string.escape

[
  "val"
  "var"
  "fun"
  "enum"
  "class"
  "object"
  "interface"
] @keyword

[
  (class_modifier)
  (member_modifier)
  (function_modifier)
  (property_modifier)
  (platform_modifier)
  (variance_modifier)
  (parameter_modifier)
  (visibility_modifier)
  (reification_modifier)
  (inheritance_modifier)
] @keyword

(type_identifier) @type

(function_declaration
  . (simple_identifier) @function)

(simple_identifier) @variable
