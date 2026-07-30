#ifndef TREE_SITTER_WASM_ASSERT_H_
#define TREE_SITTER_WASM_ASSERT_H_

#ifdef NDEBUG
#define assert(e) ((void)0)
#else
#define assert(expression) ((expression) ? (void)0 : __builtin_trap())
#endif

#endif // TREE_SITTER_WASM_ASSERT_H_
