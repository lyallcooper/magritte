[
  (quoted_argument)
  (bracket_argument)
] @string

(variable_ref) @variable.special
(variable) @variable

[
  (bracket_comment)
  (line_comment)
] @comment

(normal_command
  (identifier) @function)

[
  "$"
  "{"
  "}"
] @punctuation.special

[
  "("
  ")"
] @punctuation.bracket

[
  (function)
  (endfunction)
  (macro)
  (endmacro)
  (if)
  (elseif)
  (else)
  (endif)
  (foreach)
  (endforeach)
  (while)
  (endwhile)
] @keyword

(escape_sequence) @string.escape
