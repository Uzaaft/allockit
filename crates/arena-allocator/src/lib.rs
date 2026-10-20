#![no_std]
#![allow(missing_docs)]
#![allow(clippy::undocumented_unsafe_blocks)]

#[cfg(feature = "std")]
extern crate std;

use core::alloc::{AllocError, Allocator, Layout};
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

pub enum ResetMode {
    FreeAll,
    RetainCapacity,
    RetainWithLimit(usize),
}

pub struct ArenaAllocator<A: Allocator> {
    parent: A,
    head: AtomicPtr<Node>,
}

struct Node {
    base: NonNull<u8>,
    layout: Layout,
    capacity: usize,
    end: AtomicUsize,
    next: *mut Node,
}

#[cfg(feature = "std")]
impl ArenaAllocator<std::alloc::Global> {
    pub const fn new() -> Self {
        Self::new_in(std::alloc::Global)
    }
}

impl<A: Allocator + Default> Default for ArenaAllocator<A> {
    fn default() -> Self {
        Self::new_in(A::default())
    }
}

impl<A: Allocator> ArenaAllocator<A> {
    pub const fn new_in(parent: A) -> Self {
        Self {
            parent,
            head: AtomicPtr::new(ptr::null_mut()),
        }
    }

    pub fn query_capacity(&self) -> usize {
        let mut node = self.head.load(Ordering::Acquire);
        let mut capacity = 0usize;
        while !node.is_null() {
            unsafe {
                capacity = capacity.saturating_add((*node).capacity);
                node = (*node).next;
            }
        }
        capacity
    }

    fn chunk(&self, capacity: usize, use_excess: bool) -> Result<*mut Node, AllocError> {
        let size = size_of::<Node>().checked_add(capacity).ok_or(AllocError)?;
        let layout = Layout::from_size_align(size, align_of::<Node>()).map_err(|_| AllocError)?;
        let block = self.parent.allocate(layout)?;
        let base = block.cast::<u8>();
        let node = base.cast::<Node>().as_ptr();
        let capacity = if use_excess {
            block.len() - size_of::<Node>()
        } else {
            capacity
        };
        unsafe {
            node.write(Node {
                base,
                layout,
                capacity,
                end: AtomicUsize::new(0),
                next: ptr::null_mut(),
            });
        }
        Ok(node)
    }

    unsafe fn release(&self, node: *mut Node) {
        unsafe { self.parent.deallocate((*node).base, (*node).layout) }
    }

    pub fn reset(&mut self, mode: ResetMode) -> bool {
        let target = match mode {
            ResetMode::FreeAll => 0,
            ResetMode::RetainCapacity => self.query_capacity(),
            ResetMode::RetainWithLimit(limit) => self.query_capacity().min(limit),
        };
        let mut node = *self.head.get_mut();
        while !node.is_null() {
            let next = unsafe { (*node).next };
            if next.is_null() && target != 0 {
                break;
            }
            unsafe { self.release(node) };
            node = next;
        }
        *self.head.get_mut() = node;
        if node.is_null() {
            return true;
        }
        unsafe {
            *(*node).end.get_mut() = 0;
            if (*node).capacity == target {
                return true;
            }
        }
        let Some(size) = size_of::<Node>().checked_add(target) else {
            return false;
        };
        let Ok(layout) = Layout::from_size_align(size, align_of::<Node>()) else {
            return false;
        };
        let (base, old_layout) = unsafe { ((*node).base, (*node).layout) };
        let resized = unsafe {
            if layout.size() >= old_layout.size() {
                self.parent.grow(base, old_layout, layout)
            } else {
                self.parent.shrink(base, old_layout, layout)
            }
        };
        if let Ok(block) = resized {
            let base = block.cast::<u8>();
            let replacement = base.cast::<Node>().as_ptr();
            unsafe {
                replacement.write(Node {
                    base,
                    layout,
                    capacity: target,
                    end: AtomicUsize::new(0),
                    next: ptr::null_mut(),
                });
            }
            *self.head.get_mut() = replacement;
            return true;
        }
        match self.chunk(target, false) {
            Ok(replacement) => {
                *self.head.get_mut() = replacement;
                unsafe { self.release(node) };
                true
            }
            Err(_) => false,
        }
    }

    unsafe fn reserve(node: *mut Node, layout: Layout) -> Option<NonNull<[u8]>> {
        unsafe {
            let node = &*node;
            let data = node.base.as_ptr().add(size_of::<Node>());
            let mut end = node.end.load(Ordering::Relaxed);
            loop {
                let address = data.addr().checked_add(end)?;
                let padding = address.wrapping_neg() & (layout.align() - 1);
                let start = end.checked_add(padding)?;
                let next = start.checked_add(layout.size())?;
                if next > node.capacity {
                    return None;
                }
                match node.end.compare_exchange_weak(
                    end,
                    next,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        return Some(NonNull::slice_from_raw_parts(
                            NonNull::new_unchecked(data.add(start)),
                            layout.size(),
                        ));
                    }
                    Err(actual) => end = actual,
                }
            }
        }
    }

    unsafe fn resize(
        &self,
        pointer: NonNull<u8>,
        old: Layout,
        new: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        if new.size() == 0 {
            unsafe { self.deallocate(pointer, old) };
            return self.allocate(new);
        }
        if old.size() != 0 && pointer.as_ptr().addr() & (new.align() - 1) == 0 {
            let head = self.head.load(Ordering::Acquire);
            if !head.is_null() {
                unsafe {
                    let node = &*head;
                    let data = node.base.as_ptr().add(size_of::<Node>());
                    if let Some(start) = pointer.as_ptr().addr().checked_sub(data.addr())
                        && let (Some(end), Some(next)) =
                            (start.checked_add(old.size()), start.checked_add(new.size()))
                        && end <= node.capacity
                        && next <= node.capacity
                        && node
                            .end
                            .compare_exchange(end, next, Ordering::AcqRel, Ordering::Relaxed)
                            .is_ok()
                    {
                        return Ok(NonNull::slice_from_raw_parts(
                            NonNull::new_unchecked(data.add(start)),
                            new.size(),
                        ));
                    }
                }
            }
            if new.size() <= old.size() {
                return Ok(NonNull::slice_from_raw_parts(pointer, new.size()));
            }
        }
        let block = self.allocate(new)?;
        unsafe {
            ptr::copy_nonoverlapping(
                pointer.as_ptr(),
                block.cast::<u8>().as_ptr(),
                old.size().min(new.size()),
            );
            self.deallocate(pointer, old);
        }
        Ok(block)
    }
}

impl<A: Allocator> Drop for ArenaAllocator<A> {
    fn drop(&mut self) {
        self.reset(ResetMode::FreeAll);
    }
}

unsafe impl<A: Allocator> Allocator for ArenaAllocator<A> {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if layout.size() == 0 {
            return Ok(NonNull::slice_from_raw_parts(
                NonNull::without_provenance(
                    core::num::NonZeroUsize::new(layout.align()).ok_or(AllocError)?,
                ),
                0,
            ));
        }
        let mut head = self.head.load(Ordering::Acquire);
        let previous = if head.is_null() {
            0
        } else {
            unsafe {
                if let Some(block) = Self::reserve(head, layout) {
                    return Ok(block);
                }
                (*head).capacity
            }
        };
        let minimum = layout
            .size()
            .checked_add(layout.align() - 1)
            .ok_or(AllocError)?;
        let capacity = previous
            .checked_add(minimum)
            .and_then(|n| n.checked_add(16))
            .and_then(|n| n.checked_add(n / 2))
            .filter(|n| *n <= (isize::MAX as usize) - size_of::<Node>() - align_of::<Node>())
            .unwrap_or(minimum);
        let node = self.chunk(capacity, true)?;
        let block = unsafe { Self::reserve(node, layout) };
        let Some(block) = block else {
            unsafe { self.release(node) };
            return Err(AllocError);
        };
        loop {
            unsafe { (*node).next = head };
            match self
                .head
                .compare_exchange_weak(head, node, Ordering::Release, Ordering::Relaxed)
            {
                Ok(_) => return Ok(block),
                Err(actual) => head = actual,
            }
        }
    }

    unsafe fn deallocate(&self, pointer: NonNull<u8>, layout: Layout) {
        if layout.size() == 0 {
            return;
        }
        let head = self.head.load(Ordering::Acquire);
        if head.is_null() {
            return;
        }
        unsafe {
            let node = &*head;
            let data = node.base.as_ptr().add(size_of::<Node>());
            if let Some(start) = pointer.as_ptr().addr().checked_sub(data.addr())
                && let Some(end) = start.checked_add(layout.size())
                && end <= node.capacity
            {
                let _ = node
                    .end
                    .compare_exchange(end, start, Ordering::Release, Ordering::Relaxed);
            }
        }
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
        let block = unsafe { self.resize(pointer, old, new)? };
        unsafe {
            block
                .cast::<u8>()
                .as_ptr()
                .add(old.size())
                .write_bytes(0, new.size() - old.size())
        };
        Ok(block)
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
