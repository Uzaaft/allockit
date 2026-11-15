//! Test that safe allocator use leaves the borrowed backing storage initialized.
#![cfg(miri)]
#![allow(missing_docs)]

use buffer_first_allocator::BufferFirstAllocator;
use std::alloc::Global;
use std::mem::MaybeUninit;

#[test]
fn backing_storage_remains_initialized() {
    let mut backing = [0u8; 1];

    {
        let allocator = BufferFirstAllocator::new_in(Global, &mut backing);
        let _ = Box::new_in(MaybeUninit::<u8>::uninit(), &allocator);
    }

    assert_eq!(backing[0], 0);
}
