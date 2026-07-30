// malloc, calloc, realloc, and free are provided by the Rust side of this
// crate (see the `c_allocator` module in src/language.rs). The bump allocator
// upstream ships here assumes an isolated external-scanner wasm instance and
// corrupts linear memory when the module shares its heap with Rust code.

#include <stdint.h>
#include <stdlib.h>

__attribute__((noreturn)) void abort(void) {
  __builtin_trap();
}
