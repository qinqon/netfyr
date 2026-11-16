//! Allocator glue for an alloc-only wasm32-wasip2 component. The test guest
//! includes this module too so both components use the same canonical ABI.

extern crate alloc;

use alloc::alloc::{Layout, alloc, dealloc, realloc};

#[global_allocator]
static ALLOCATOR: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

// LLVM emits memcmp/memcpy/etc. calls even for no_std Rust. Pull only the
// referenced memory routines from the target's bundled static C library.
#[link(name = "c")]
unsafe extern "C" {}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}

/// wasm32-wasip2 normally gets this export from std. wit-bindgen intentionally
/// does not supply it for that target, so an alloc-only component provides it.
///
/// # Safety
/// When old_len is nonzero, old_ptr must refer to a live allocation from this
/// allocator with exactly that size and alignment, as required by the canonical
/// ABI. No references to that allocation may survive this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cabi_realloc(
    old_ptr: *mut u8,
    old_len: usize,
    align: usize,
    new_len: usize,
) -> *mut u8 {
    let layout = |size| {
        Layout::from_size_align(size, align).unwrap_or_else(|_| core::arch::wasm32::unreachable())
    };
    let new_layout = layout(new_len);
    if new_len == 0 {
        if old_len != 0 {
            // SAFETY: the canonical ABI caller supplies the original layout.
            unsafe { dealloc(old_ptr, layout(old_len)) };
        }
        return align as *mut u8;
    }
    // SAFETY: new_layout is nonempty and valid; when reallocating, the caller
    // supplies the original allocation and its matching layout.
    let ptr = unsafe {
        if old_len == 0 {
            alloc(new_layout)
        } else {
            realloc(old_ptr, layout(old_len), new_len)
        }
    };
    if ptr.is_null() {
        core::arch::wasm32::unreachable();
    }
    ptr
}
