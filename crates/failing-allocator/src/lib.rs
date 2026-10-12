//! An allocator wrapper that fails after a configurable number of allocations or resizes.
//!
//! Useful for testing that out-of-memory conditions are handled correctly. Modeled on Zig's
//! `std.testing.FailingAllocator`.

#![no_std]

#[cfg(feature = "std")]
extern crate std;

use core::alloc::{AllocError, Allocator, Layout};
use core::cell::Cell;
use core::ptr;

/// When a [`FailingAllocator`] starts failing.
///
/// The default is very unlikely to fail.
pub struct Config {
    /// The number of successful allocations to expect from this allocator. The next allocation
    /// will fail.
    pub fail_index: usize,
    /// The number of successful resizes to expect from this allocator. The next resize will fail.
    pub resize_fail_index: usize,
}

/// Wraps an allocator and fails after a configured number of allocations or resizes.
///
/// Counts allocations, deallocations, and bytes so tests can assert on them. Not thread-safe:
/// the counters use [`Cell`].
pub struct FailingAllocator<A>
where
    A: Allocator,
{
    alloc_index: Cell<usize>,
    resize_index: Cell<usize>,
    internal_allocator: A,
    allocated_bytes: Cell<usize>,
    freed_bytes: Cell<usize>,
    allocations: Cell<usize>,
    deallocations: Cell<usize>,
    has_induced_errors: Cell<bool>,
    fail_index: Cell<usize>,
    resize_fail_index: Cell<usize>,
}

#[cfg(feature = "std")]
impl FailingAllocator<std::alloc::Global> {
    /// Creates a failing allocator backed by the global allocator.
    pub const fn new(config: Config) -> Self {
        Self::new_in(std::alloc::Global, config)
    }
}

impl<A: Allocator> FailingAllocator<A> {
    /// Creates a failing allocator backed by `internal_allocator`.
    pub const fn new_in(internal_allocator: A, config: Config) -> Self {
        Self {
            alloc_index: Cell::new(0),
            resize_index: Cell::new(0),
            internal_allocator,
            allocated_bytes: Cell::new(0),
            freed_bytes: Cell::new(0),
            allocations: Cell::new(0),
            deallocations: Cell::new(0),
            has_induced_errors: Cell::new(false),
            fail_index: Cell::new(config.fail_index),
            resize_fail_index: Cell::new(config.resize_fail_index),
        }
    }

    /// Number of successful allocations.
    pub fn allocations(&self) -> usize {
        self.allocations.get()
    }

    /// Number of deallocations.
    pub fn deallocations(&self) -> usize {
        self.deallocations.get()
    }

    /// Total bytes allocated, including growth from resizes.
    pub fn allocated_bytes(&self) -> usize {
        self.allocated_bytes.get()
    }

    /// Total bytes freed, including shrinkage from resizes.
    pub fn freed_bytes(&self) -> usize {
        self.freed_bytes.get()
    }

    /// Whether an allocation failure has been injected.
    pub fn has_induced_failure(&self) -> bool {
        self.has_induced_errors.get()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            fail_index: usize::MAX,
            resize_fail_index: usize::MAX,
        } // never fail, like Zig
    }
}

impl<A: Allocator + Default> Default for FailingAllocator<A> {
    fn default() -> Self {
        Self::new_in(A::default(), Config::default())
    }
}

// SAFETY: every block is allocated, resized, and freed by `internal_allocator` with the
// caller's arguments unchanged, so its guarantees carry over; injected failures return
// `Err` without handing out memory.
unsafe impl<A> Allocator for FailingAllocator<A>
where
    A: Allocator,
{
    fn allocate(&self, layout: Layout) -> Result<ptr::NonNull<[u8]>, AllocError> {
        if self.alloc_index == self.fail_index {
            self.has_induced_errors.set(true);
            return Err(AllocError);
        }
        let ptr = self.internal_allocator.allocate(layout)?;
        self.allocated_bytes
            .set(self.allocated_bytes.get() + layout.size());
        self.allocations.set(self.allocations.get() + 1);
        self.alloc_index.set(self.alloc_index.get() + 1);

        Ok(ptr)
    }

    fn allocate_zeroed(&self, layout: Layout) -> Result<ptr::NonNull<[u8]>, AllocError> {
        if self.alloc_index == self.fail_index {
            self.has_induced_errors.set(true);
            return Err(AllocError);
        }
        let ptr = self.internal_allocator.allocate_zeroed(layout)?;
        self.allocated_bytes
            .set(self.allocated_bytes.get() + layout.size());
        self.allocations.set(self.allocations.get() + 1);
        self.alloc_index.set(self.alloc_index.get() + 1);

        Ok(ptr)
    }

    unsafe fn deallocate(&self, ptr: core::ptr::NonNull<u8>, layout: core::alloc::Layout) {
        // SAFETY: the caller guarantees `ptr` is currently allocated by this allocator
        // with `layout`. every block we hand out comes from `internal_allocator`, so
        // forwarding the same `ptr` and `layout` to it upholds its contract.
        unsafe {
            self.internal_allocator.deallocate(ptr, layout);
        }
        self.deallocations.set(self.deallocations.get() + 1);
        self.freed_bytes.set(self.freed_bytes.get() + layout.size());
    }

    unsafe fn grow(
        &self,
        ptr: ptr::NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<ptr::NonNull<[u8]>, AllocError> {
        debug_assert!(
            new_layout.size() >= old_layout.size(),
            "`new_layout.size()` must be greater than or equal to `old_layout.size()`"
        );

        if self.resize_index == self.resize_fail_index {
            return Err(AllocError);
        }

        // SAFETY: the caller upholds `grow`'s contract. `ptr` was allocated by
        // `internal_allocator`, so forwarding upholds the contract.
        let new_ptr = unsafe { self.internal_allocator.grow(ptr, old_layout, new_layout)? };

        self.allocated_bytes
            .set(self.allocated_bytes.get() + (new_layout.size() - old_layout.size()));

        self.resize_index.set(self.resize_index.get() + 1);

        Ok(new_ptr)
    }

    unsafe fn grow_zeroed(
        &self,
        ptr: ptr::NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<ptr::NonNull<[u8]>, AllocError> {
        debug_assert!(
            new_layout.size() >= old_layout.size(),
            "`new_layout.size()` must be greater than or equal to `old_layout.size()`"
        );

        if self.resize_index == self.resize_fail_index {
            return Err(AllocError);
        }

        // SAFETY: the caller upholds `grow_zeroed`'s contract. `ptr` was allocated by
        // `internal_allocator`, so forwarding upholds the contract.
        let new_ptr = unsafe {
            self.internal_allocator
                .grow_zeroed(ptr, old_layout, new_layout)?
        };

        self.allocated_bytes
            .set(self.allocated_bytes.get() + (new_layout.size() - old_layout.size()));

        self.resize_index.set(self.resize_index.get() + 1);

        Ok(new_ptr)
    }

    unsafe fn shrink(
        &self,
        ptr: ptr::NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<ptr::NonNull<[u8]>, AllocError> {
        debug_assert!(
            new_layout.size() <= old_layout.size(),
            "`new_layout.size()` must be smaller than or equal to `old_layout.size()`"
        );

        if self.resize_index == self.resize_fail_index {
            return Err(AllocError);
        }

        // SAFETY: the caller upholds `shrink`'s contract. `ptr` was allocated by
        // `internal_allocator`, so forwarding upholds the contract.
        let new_ptr = unsafe {
            self.internal_allocator
                .shrink(ptr, old_layout, new_layout)?
        };

        self.freed_bytes
            .set(self.freed_bytes.get() + (old_layout.size() - new_layout.size()));
        self.resize_index.set(self.resize_index.get() + 1);

        Ok(new_ptr)
    }
}
