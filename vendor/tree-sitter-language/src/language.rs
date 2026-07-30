#![no_std]
/// `LanguageFn` wraps a C function that returns a pointer to a tree-sitter grammar.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct LanguageFn(unsafe extern "C" fn() -> *const ());

impl LanguageFn {
    /// Creates a [`LanguageFn`].
    ///
    /// # Safety
    ///
    /// Only call this with language functions generated from grammars
    /// by the Tree-sitter CLI.
    pub const unsafe fn from_raw(f: unsafe extern "C" fn() -> *const ()) -> Self {
        Self(f)
    }

    /// Gets the function wrapped by this [`LanguageFn`].
    #[must_use]
    pub const fn into_raw(self) -> unsafe extern "C" fn() -> *const () {
        self.0
    }
}

// The C sources in `wasm/src` become the libc for every wasm32-unknown-unknown
// build of the tree-sitter runtime and its grammars. Upstream implements
// `malloc` there with a bump allocator that assumes the module is an isolated
// external-scanner instance owning all of linear memory starting from address
// zero; linked into a binary that shares its memory with Rust, the first
// allocation clobbers the data segment. Provide the C allocation entry points
// from Rust's global allocator instead, prefixing each allocation with a
// header that records its size for `free`/`realloc`.
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod c_allocator {
    extern crate alloc;

    use alloc::alloc::{alloc, alloc_zeroed, dealloc, realloc as rust_realloc};
    use core::alloc::Layout;

    // Holds the size header while keeping the pointer handed to C aligned to
    // max_align_t.
    const HEADER: usize = 16;

    fn layout(size: usize) -> Option<Layout> {
        Layout::from_size_align(size.checked_add(HEADER)?, HEADER).ok()
    }

    // Never panic in these: panicking allocates, and a signature mismatch with
    // the C caller would make unwinding undefined anyway. Report failure the
    // way C expects, with a null return.
    #[no_mangle]
    unsafe extern "C" fn malloc(size: usize) -> *mut u8 {
        let Some(layout) = layout(size) else {
            return core::ptr::null_mut();
        };
        let base = unsafe { alloc(layout) };
        if base.is_null() {
            return base;
        }
        unsafe {
            (base as *mut usize).write(size);
            base.add(HEADER)
        }
    }

    #[no_mangle]
    unsafe extern "C" fn calloc(count: usize, size: usize) -> *mut u8 {
        let Some(total) = count.checked_mul(size) else {
            return core::ptr::null_mut();
        };
        let Some(layout) = layout(total) else {
            return core::ptr::null_mut();
        };
        let base = unsafe { alloc_zeroed(layout) };
        if base.is_null() {
            return base;
        }
        unsafe {
            (base as *mut usize).write(total);
            base.add(HEADER)
        }
    }

    #[no_mangle]
    unsafe extern "C" fn realloc(ptr: *mut u8, new_size: usize) -> *mut u8 {
        if ptr.is_null() {
            return unsafe { malloc(new_size) };
        }
        let Some(new_layout) = layout(new_size) else {
            return core::ptr::null_mut();
        };
        unsafe {
            let base = ptr.sub(HEADER);
            let old_size = (base as *const usize).read();
            let old_layout = layout(old_size).unwrap_unchecked();
            let new_base = rust_realloc(base, old_layout, new_layout.size());
            if new_base.is_null() {
                return new_base;
            }
            (new_base as *mut usize).write(new_size);
            new_base.add(HEADER)
        }
    }

    #[no_mangle]
    unsafe extern "C" fn free(ptr: *mut u8) {
        if ptr.is_null() {
            return;
        }
        unsafe {
            let base = ptr.sub(HEADER);
            let size = (base as *const usize).read();
            let layout = layout(size).unwrap_unchecked();
            dealloc(base, layout);
        }
    }
}
