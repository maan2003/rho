; Keep id text in the buffer and copy/paste stream. Highlighting applies the
; strong dim; this context capture lets Zed suppress drawer punctuation only.
((ERROR) @conceal.context
  (#match? @conceal.context "^:id:"))
