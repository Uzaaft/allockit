#![no_std]
#![allow(missing_docs)]
#![allow(clippy::undocumented_unsafe_blocks)]
#![cfg(all(feature = "std", any(target_os = "macos", target_os = "linux")))]

extern crate std;

use core::alloc::{AllocError, Allocator, Layout};
use core::cell::{Cell, UnsafeCell};
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use page_allocator::PageAllocator;

const MAX_SHARDS: usize = 128;
const MIN_CLASS: usize = size_of::<usize>().trailing_zeros() as usize;
const CLASSES: usize = usize::BITS as usize - MIN_CLASS;

#[derive(Clone, Copy, Debug, Default)]
pub struct SmpAllocator;

struct State {
    next: [usize; CLASSES],
    free: [usize; CLASSES],
}

#[repr(align(128))]
struct Shard {
    locked: AtomicBool,
    state: UnsafeCell<State>,
}

unsafe impl Sync for Shard {}

static SHARDS: [Shard; MAX_SHARDS] = [const {
    Shard {
        locked: AtomicBool::new(false),
        state: UnsafeCell::new(State {
            next: [0; CLASSES],
            free: [0; CLASSES],
        }),
    }
}; MAX_SHARDS];
static CPU_COUNT: AtomicUsize = AtomicUsize::new(0);
static SLAB_SIZE: AtomicUsize = AtomicUsize::new(0);

std::thread_local! {
    static PREFERRED: Cell<usize> = const { Cell::new(0) };
}

struct Guard(&'static Shard);

impl Guard {
    fn state(&mut self) -> &mut State {
        unsafe { &mut *self.0.state.get() }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.locked.store(false, Ordering::Release);
    }
}

fn cpu_count() -> usize {
    let cached = CPU_COUNT.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    let count = usize::try_from(unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) })
        .unwrap_or(MAX_SHARDS)
        .clamp(1, MAX_SHARDS);
    match CPU_COUNT.compare_exchange(0, count, Ordering::Relaxed, Ordering::Relaxed) {
        Ok(_) => count,
        Err(count) => count,
    }
}

fn slab_size() -> Result<usize, AllocError> {
    let cached = SLAB_SIZE.load(Ordering::Relaxed);
    if cached != 0 {
        return Ok(cached);
    }
    let page =
        usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).map_err(|_| AllocError)?;
    if !page.is_power_of_two() || page > isize::MAX as usize {
        return Err(AllocError);
    }
    let size = page.max(64 * 1024);
    SLAB_SIZE.store(size, Ordering::Relaxed);
    Ok(size)
}

fn lock(rotate: bool) -> Guard {
    let count = cpu_count();
    let mut index = PREFERRED.try_with(Cell::get).unwrap_or(0);
    if rotate {
        index = (index + 1) % count;
    }
    loop {
        let shard = &SHARDS[index];
        if shard
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            let _ = PREFERRED.try_with(|preferred| preferred.set(index));
            return Guard(shard);
        }
        index = (index + 1) % count;
        core::hint::spin_loop();
    }
}

fn class(layout: Layout) -> usize {
    let size = layout.size().max(layout.align()).max(size_of::<usize>());
    (usize::BITS - (size - 1).leading_zeros()) as usize - MIN_CLASS
}

fn small(class: usize, slab: usize) -> bool {
    class + MIN_CLASS < slab.trailing_zeros() as usize
}

fn block(address: usize, size: usize) -> NonNull<[u8]> {
    NonNull::slice_from_raw_parts(
        unsafe { NonNull::new_unchecked(ptr::with_exposed_provenance_mut(address)) },
        size,
    )
}

impl SmpAllocator {
    unsafe fn resize(
        &self,
        pointer: NonNull<u8>,
        old: Layout,
        new: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        if old.size() != 0 && new.size() != 0 {
            let slab = slab_size()?;
            let old_class = class(old);
            let new_class = class(new);
            if small(old_class, slab) && old_class == new_class {
                return Ok(block(pointer.as_ptr().addr(), new.size()));
            }
            if !small(old_class, slab) && !small(new_class, slab) {
                return if new.size() >= old.size() {
                    unsafe { PageAllocator.grow(pointer, old, new) }
                } else {
                    unsafe { PageAllocator.shrink(pointer, old, new) }
                };
            }
        }
        let allocation = self.allocate(new)?;
        let copied = old.size().min(new.size());
        unsafe {
            if copied != 0 {
                ptr::copy_nonoverlapping(
                    ptr::with_exposed_provenance::<u8>(pointer.as_ptr().addr()),
                    allocation.cast::<u8>().as_ptr(),
                    copied,
                );
            }
            self.deallocate(pointer, old);
        }
        Ok(allocation)
    }
}

unsafe impl Allocator for SmpAllocator {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if layout.size() == 0 {
            return Ok(NonNull::slice_from_raw_parts(layout.dangling_ptr(), 0));
        }
        let slab = slab_size()?;
        let class = class(layout);
        if !small(class, slab) {
            return PageAllocator.allocate(layout);
        }
        let slot = 1usize << (class + MIN_CLASS);
        let mut guard = lock(false);
        for search in 0..=1 {
            let state = guard.state();
            let free = state.free[class];
            if free != 0 {
                state.free[class] = unsafe { ptr::with_exposed_provenance::<usize>(free).read() };
                return Ok(block(free, layout.size()));
            }
            let next = state.next[class];
            if next & (slab - 1) != 0 {
                state.next[class] = next + slot;
                return Ok(block(next, layout.size()));
            }
            if search == 1 {
                let slab_layout = Layout::from_size_align(slab, slab).map_err(|_| AllocError)?;
                let allocation = PageAllocator.allocate(slab_layout)?;
                let address = allocation.cast::<u8>().as_ptr().addr();
                state.next[class] = address + slot;
                return Ok(block(address, layout.size()));
            }
            drop(guard);
            guard = lock(true);
        }
        Err(AllocError)
    }

    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let allocation = self.allocate(layout)?;
        if layout.size() != 0 {
            unsafe {
                allocation
                    .cast::<u8>()
                    .as_ptr()
                    .write_bytes(0, layout.size())
            };
        }
        Ok(allocation)
    }

    unsafe fn deallocate(&self, pointer: NonNull<u8>, layout: Layout) {
        if layout.size() == 0 {
            return;
        }
        let slab = SLAB_SIZE.load(Ordering::Relaxed);
        let class = class(layout);
        if !small(class, slab) {
            unsafe { PageAllocator.deallocate(pointer, layout) };
            return;
        }
        let address = pointer.as_ptr().addr();
        let mut guard = lock(false);
        let state = guard.state();
        unsafe { ptr::with_exposed_provenance_mut::<usize>(address).write(state.free[class]) };
        state.free[class] = address;
    }

    unsafe fn grow(
        &self,
        pointer: NonNull<u8>,
        old: Layout,
        new: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        unsafe { self.resize(pointer, old, new) }
    }

    unsafe fn grow_zeroed(
        &self,
        pointer: NonNull<u8>,
        old: Layout,
        new: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        let allocation = unsafe { self.resize(pointer, old, new)? };
        if new.size() > old.size() {
            unsafe {
                allocation
                    .cast::<u8>()
                    .as_ptr()
                    .add(old.size())
                    .write_bytes(0, new.size() - old.size());
            }
        }
        Ok(allocation)
    }

    unsafe fn shrink(
        &self,
        pointer: NonNull<u8>,
        old: Layout,
        new: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        unsafe { self.resize(pointer, old, new) }
    }
}
