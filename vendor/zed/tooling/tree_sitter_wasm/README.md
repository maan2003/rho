# Tree-sitter wasm C headers

These minimal libc headers are copied from tree-sitter-language 0.1.7
(`crates/language/wasm/include` in tree-sitter v0.26.9, MIT licensed).
They let clang compile statically linked grammar `parser.c` files for
`wasm32-unknown-unknown`. The runtime's build script discovers the same
headers through tree-sitter-language's Cargo metadata.
