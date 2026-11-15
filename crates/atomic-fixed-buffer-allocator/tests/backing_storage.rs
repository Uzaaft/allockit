#![allow(missing_docs)]

use atomic_fixed_buffer_allocator::AtomicFixedBufferAllocator;
use std::mem::MaybeUninit;

/// Test that borrowed backing storage can become uninitialized and be initialized again.
#[test]
fn backing_storage_can_be_reinitialized() {
    let mut backing = [MaybeUninit::new(0u8); 1];

    {
        let allocator = AtomicFixedBufferAllocator::new(&mut backing);
        let _ = Box::new_in(MaybeUninit::<u8>::uninit(), &allocator);
    }

    assert_eq!(*backing[0].write(42), 42);
}
