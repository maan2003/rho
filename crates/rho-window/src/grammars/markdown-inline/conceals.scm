(emphasis
  (emphasis_delimiter) @conceal) @conceal.context

(strong_emphasis
  (emphasis_delimiter) @conceal) @conceal.context

(code_span
  (code_span_delimiter) @conceal) @conceal.context

(strikethrough
  (emphasis_delimiter) @conceal) @conceal.context

(inline_link
  [
    "["
    "]"
    "("
    (link_destination)
    (link_title)
    ")"
  ] @conceal) @conceal.context
