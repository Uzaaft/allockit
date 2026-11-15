#![no_std]
#![allow(missing_docs)]

#[cfg(feature = "std")]
extern crate std;

use core::alloc::{AllocError, Allocator, Layout};
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicUsize, Ordering};

pub struct AtomicFixedBufferAllocator<'a> {
    end_index: AtomicUsize,
    base: NonNull<u8>,
    len: usize,
    _buffer: PhantomData<&'a mut [MaybeUninit<u8>]>,
}

#[allow(clippy::undocumented_unsafe_blocks)]
unsafe impl Send for AtomicFixedBufferAllocator<'_> {}

#[allow(clippy::undocumented_unsafe_blocks)]
unsafe impl Sync for AtomicFixedBufferAllocator<'_> {}

#[allow(clippy::undocumented_unsafe_blocks)]
impl<'a> AtomicFixedBufferAllocator<'a> {
    pub fn new(buffer: &'a mut [MaybeUninit<u8>]) -> Self {
        Self {
            end_index: AtomicUsize::new(0),
            len: buffer.len(),
            base: NonNull::from(buffer).cast(),
            _buffer: PhantomData,
        }
    }

    unsafe fn resize(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        if old_layout.size() != 0
            && new_layout.size() != 0
            && ptr.addr().get() & (new_layout.align() - 1) == 0
        {
            let start = ptr.addr().get() - self.base.addr().get();
            let old_end = start + old_layout.size();
            if new_layout.size() <= old_layout.size() {
                let _ = self.end_index.compare_exchange(
                    old_end,
                    start + new_layout.size(),
                    Ordering::Release,
                    Ordering::Relaxed,
                );
                return Ok(NonNull::slice_from_raw_parts(
                    unsafe { self.base.add(start) },
                    new_layout.size(),
                ));
            }
            if let Some(new_end) = start.checked_add(new_layout.size())
                && new_end <= self.len
                && self
                    .end_index
                    .compare_exchange(old_end, new_end, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
            {
                return Ok(NonNull::slice_from_raw_parts(
                    unsafe { self.base.add(start) },
                    new_layout.size(),
                ));
            }
        }

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
unsafe impl Allocator for AtomicFixedBufferAllocator<'_> {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if layout.size() == 0 {
            let ptr =
                unsafe { NonNull::new_unchecked(ptr::without_provenance_mut(layout.align())) };
            return Ok(NonNull::slice_from_raw_parts(ptr, 0));
        }

        let mut end = self.end_index.load(Ordering::Relaxed);
        loop {
            let address = self.base.addr().get().checked_add(end).ok_or(AllocError)?;
            let padding = address.wrapping_neg() & (layout.align() - 1);
            let start = end.checked_add(padding).ok_or(AllocError)?;
            let new_end = start.checked_add(layout.size()).ok_or(AllocError)?;
            if new_end > self.len {
                return Err(AllocError);
            }
            match self.end_index.compare_exchange_weak(
                end,
                new_end,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Ok(NonNull::slice_from_raw_parts(
                        unsafe { self.base.add(start) },
                        layout.size(),
                    ));
                }
                Err(current) => end = current,
            }
        }
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        if layout.size() == 0 {
            return;
        }
        let start = ptr.addr().get() - self.base.addr().get();
        let _ = self.end_index.compare_exchange(
            start + layout.size(),
            start,
            Ordering::Release,
            Ordering::Relaxed,
        );
    }

    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let ptr = self.allocate(layout)?;
        if !ptr.is_empty() {
            unsafe { ptr.cast::<u8>().as_ptr().write_bytes(0, ptr.len()) };
        }
        Ok(ptr)
    }

    unsafe fn grow(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        unsafe { self.resize(ptr, old_layout, new_layout) }
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
        unsafe { self.resize(ptr, old_layout, new_layout) }
    }
}
