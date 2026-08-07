(headline
  (stars) @punctuation.special
  (item) @title)

(item .
  (expr) @keyword
  (#any-of? @keyword "TODO" "STAFFED" "DONE" "DISCARDED"))

; Identity remains real selectable text. Rendering it like a comment makes it
; unobtrusive without giving editing code permission to delete it.
((ERROR) @comment
  (#match? @comment "^:id:"))
