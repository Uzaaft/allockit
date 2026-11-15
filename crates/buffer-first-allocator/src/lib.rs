#![no_std]
#![allow(missing_docs)]

#[cfg(feature = "std")]
extern crate std;

use core::alloc::{AllocError, Allocator, Layout};
use core::mem::MaybeUninit;
use core::ptr::{self, NonNull};
use fixed_buffer_allocator::FixedBufferAllocator;

pub struct BufferFirstAllocator<'a, A: Allocator> {
    fixed_buffer_allocator: FixedBufferAllocator<'a>,
    fallback_allocator: A,
}

#[cfg(feature = "std")]
impl<'a> BufferFirstAllocator<'a, std::alloc::Global> {
    pub fn new(buffer: &'a mut [MaybeUninit<u8>]) -> Self {
        Self::new_in(std::alloc::Global, buffer)
    }
}

#[allow(clippy::undocumented_unsafe_blocks)]
impl<'a, A: Allocator> BufferFirstAllocator<'a, A> {
    pub fn new_in(fallback_allocator: A, buffer: &'a mut [MaybeUninit<u8>]) -> Self {
        Self {
            fixed_buffer_allocator: FixedBufferAllocator::new(buffer),
            fallback_allocator,
        }
    }

    unsafe fn relocate(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        let new_ptr = self.allocate(new_layout)?;
        let copy_len = old_layout.size().min(new_layout.size());
        unsafe {
            if copy_len != 0 {
                ptr::copy_nonoverlapping(ptr.as_ptr(), new_ptr.cast::<u8>().as_ptr(), copy_len);
            }
            self.deallocate(ptr, old_layout);
        }
        Ok(new_ptr)
    }
}

#[allow(clippy::undocumented_unsafe_blocks)]
unsafe impl<A: Allocator> Allocator for BufferFirstAllocator<'_, A> {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.fixed_buffer_allocator
            .allocate(layout)
            .or_else(|_| self.fallback_allocator.allocate(layout))
    }

    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.fixed_buffer_allocator
            .allocate_zeroed(layout)
            .or_else(|_| self.fallback_allocator.allocate_zeroed(layout))
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        if layout.size() == 0 {
            return;
        }
        unsafe {
            if self.fixed_buffer_allocator.owns_ptr(ptr) {
                self.fixed_buffer_allocator.deallocate(ptr, layout);
            } else {
                self.fallback_allocator.deallocate(ptr, layout);
            }
        }
    }

    unsafe fn grow(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        if old_layout.size() == 0 {
            return self.allocate(new_layout);
        }
        unsafe {
            let result = if self.fixed_buffer_allocator.owns_ptr(ptr) {
                self.fixed_buffer_allocator
                    .grow(ptr, old_layout, new_layout)
            } else {
                self.fallback_allocator.grow(ptr, old_layout, new_layout)
            };
            result.or_else(|_| self.relocate(ptr, old_layout, new_layout))
        }
    }

    unsafe fn grow_zeroed(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        let new_ptr = unsafe { self.grow(ptr, old_layout, new_layout)? };
        if new_ptr.len() > old_layout.size() {
            unsafe {
                new_ptr
                    .cast::<u8>()
                    .as_ptr()
                    .add(old_layout.size())
                    .write_bytes(0, new_ptr.len() - old_layout.size());
            }
        }
        Ok(new_ptr)
    }

    unsafe fn shrink(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        if new_layout.size() == 0 {
            return unsafe { self.relocate(ptr, old_layout, new_layout) };
        }
        unsafe {
            let result = if self.fixed_buffer_allocator.owns_ptr(ptr) {
                self.fixed_buffer_allocator
                    .shrink(ptr, old_layout, new_layout)
            } else {
                self.fallback_allocator.shrink(ptr, old_layout, new_layout)
            };
            result.or_else(|_| self.relocate(ptr, old_layout, new_layout))
        }
    }
}
