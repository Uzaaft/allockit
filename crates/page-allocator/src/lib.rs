#![no_std]
#![allow(missing_docs)]
#![allow(clippy::undocumented_unsafe_blocks)]
#![cfg(any(target_os = "macos", target_os = "linux"))]

#[cfg(feature = "std")]
extern crate std;

// TODO: Windows support.

use core::alloc::{AllocError, Allocator, Layout};
use core::ptr::{self, NonNull};

#[derive(Clone, Copy, Debug, Default)]
pub struct PageAllocator;

fn page_size() -> Result<usize, AllocError> {
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let size = usize::try_from(size).map_err(|_| AllocError)?;
    if size.is_power_of_two() {
        Ok(size)
    } else {
        Err(AllocError)
    }
}

fn rounded_size(size: usize, page: usize) -> Result<usize, AllocError> {
    size.checked_add(page - 1)
        .map(|size| size & !(page - 1))
        .filter(|&size| size <= isize::MAX as usize)
        .ok_or(AllocError)
}

impl PageAllocator {
    unsafe fn resize(
        &self,
        pointer: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        if old_layout.size() == 0 {
            return self.allocate(new_layout);
        }
        if new_layout.size() == 0 {
            unsafe { self.deallocate(pointer, old_layout) };
            return self.allocate(new_layout);
        }

        let page = page_size()?;
        let old_size = rounded_size(old_layout.size(), page)?;
        let new_size = rounded_size(new_layout.size(), page)?;
        let address = pointer.as_ptr().addr();
        let base = ptr::with_exposed_provenance_mut::<u8>(address);
        if address & (new_layout.align() - 1) == 0 {
            if new_size <= old_size {
                if new_size < old_size
                    && unsafe { libc::munmap(base.add(new_size).cast(), old_size - new_size) } != 0
                {
                    return Err(AllocError);
                }
                return Ok(NonNull::slice_from_raw_parts(
                    unsafe { NonNull::new_unchecked(base) },
                    new_layout.size(),
                ));
            }

            #[cfg(target_os = "linux")]
            if new_layout.align() <= page {
                let remapped =
                    unsafe { libc::mremap(base.cast(), old_size, new_size, libc::MREMAP_MAYMOVE) };
                if remapped != libc::MAP_FAILED {
                    remapped.expose_provenance();
                    return Ok(NonNull::slice_from_raw_parts(
                        unsafe { NonNull::new_unchecked(remapped.cast()) },
                        new_layout.size(),
                    ));
                }
            }
        }

        let allocation = self.allocate(new_layout)?;
        unsafe {
            ptr::copy_nonoverlapping(
                base,
                allocation.cast::<u8>().as_ptr(),
                old_layout.size().min(new_layout.size()),
            );
            self.deallocate(pointer, old_layout);
        }
        Ok(allocation)
    }
}

unsafe impl Allocator for PageAllocator {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if layout.size() == 0 {
            return Ok(NonNull::slice_from_raw_parts(layout.dangling_ptr(), 0));
        }
        let page = page_size()?;
        let size = rounded_size(layout.size(), page)?;
        let mapped_size = size
            .checked_add(layout.align().saturating_sub(page))
            .filter(|&size| size <= isize::MAX as usize)
            .ok_or(AllocError)?;
        let mapping = unsafe {
            libc::mmap(
                ptr::null_mut(),
                mapped_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if mapping == libc::MAP_FAILED {
            return Err(AllocError);
        }
        let address = mapping.expose_provenance();
        let Some(aligned) = address
            .checked_add(layout.align() - 1)
            .map(|address| address & !(layout.align() - 1))
            .filter(|&address| address != 0 && address.checked_add(size).is_some())
        else {
            unsafe { libc::munmap(mapping, mapped_size) };
            return Err(AllocError);
        };
        let prefix = aligned - address;
        let suffix = mapped_size - prefix - size;
        let base = ptr::with_exposed_provenance_mut::<u8>(aligned);
        unsafe {
            if prefix != 0 {
                libc::munmap(mapping, prefix);
            }
            if suffix != 0 {
                libc::munmap(base.add(size).cast(), suffix);
            }
        }
        Ok(NonNull::slice_from_raw_parts(
            unsafe { NonNull::new_unchecked(base) },
            layout.size(),
        ))
    }

    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.allocate(layout)
    }

    unsafe fn deallocate(&self, pointer: NonNull<u8>, layout: Layout) {
        if layout.size() != 0 {
            unsafe { libc::munmap(pointer.as_ptr().cast(), layout.size()) };
        }
    }

    unsafe fn grow(
        &self,
        pointer: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        unsafe { self.resize(pointer, old_layout, new_layout) }
    }

    unsafe fn grow_zeroed(
        &self,
        pointer: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        let allocation = unsafe { self.resize(pointer, old_layout, new_layout)? };
        unsafe {
            allocation
                .cast::<u8>()
                .as_ptr()
                .add(old_layout.size())
                .write_bytes(0, new_layout.size() - old_layout.size());
        }
        Ok(allocation)
    }

    unsafe fn shrink(
        &self,
        pointer: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        unsafe { self.resize(pointer, old_layout, new_layout) }
    }
}
