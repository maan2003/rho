#ifndef TREE_SITTER_WASM_WCHAR_H_
#define TREE_SITTER_WASM_WCHAR_H_

#include <stdbool.h>
#include <ctype.h>
#include <string.h>
#include <wctype.h>

static inline wint_t towlower(wint_t wch) {
  if (wch >= L'A' && wch <= L'Z') {
    return wch + (L'a' - L'A');
  }
  return wch;
}

#endif // TREE_SITTER_WASM_WCHAR_H_
