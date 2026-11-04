#![no_std]
#![allow(missing_docs)]
#![allow(clippy::undocumented_unsafe_blocks)]

#[cfg(feature = "std")]
extern crate std;

use core::alloc::{AllocError, Allocator, Layout};
use core::cell::UnsafeCell;
use core::ops::BitXor;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicBool, Ordering};

const BUCKET_SIZE: usize = 8192;
const GUARD_SIZE: usize = size_of::<u64>();

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub check_write_after_free: bool,
    pub canary: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            check_write_after_free: true,
            canary: 0x85df_f10f,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LeakReport {
    pub allocations: usize,
    pub bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckError {
    MetadataCorrupted { address: usize },
    GuardCorrupted { address: usize },
    FreedMemoryModified { address: usize },
}

pub struct SafeAllocator<A: Allocator> {
    parent: A,
    options: Options,
    locked: AtomicBool,
    state: UnsafeCell<State>,
}

struct State {
    first: *mut Bucket,
    current: *mut Bucket,
    table: *mut Entry,
    capacity: usize,
    occupied: usize,
    used: usize,
    leaks: LeakReport,
}

struct Bucket {
    next: *mut Bucket,
    prev: *mut Bucket,
    layout: Layout,
    fill: usize,
    live: usize,
    first: *mut Record,
}

struct Record {
    next: *mut Record,
    bucket: *mut Bucket,
    user: *mut u8,
    size: usize,
    align: usize,
    freed: u8,
    checksum: u64,
}

#[derive(Clone, Copy)]
struct Entry {
    address: usize,
    record: *mut Record,
}

struct Lock<'a>(&'a AtomicBool);

impl Drop for Lock<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

unsafe impl<A: Allocator + Send> Send for SafeAllocator<A> {}
unsafe impl<A: Allocator + Sync> Sync for SafeAllocator<A> {}

#[cfg(feature = "std")]
impl SafeAllocator<std::alloc::Global> {
    pub fn new() -> Self {
        Self::new_in(std::alloc::Global)
    }
}

impl<A: Allocator + Default> Default for SafeAllocator<A> {
    fn default() -> Self {
        Self::new_in(A::default())
    }
}

impl<A: Allocator> SafeAllocator<A> {
    pub fn new_in(parent: A) -> Self {
        Self::with_options_in(parent, Options::default())
    }

    pub const fn with_options_in(parent: A, options: Options) -> Self {
        Self {
            parent,
            options,
            locked: AtomicBool::new(false),
            state: UnsafeCell::new(State {
                first: ptr::null_mut(),
                current: ptr::null_mut(),
                table: ptr::null_mut(),
                capacity: 0,
                occupied: 0,
                used: 0,
                leaks: LeakReport {
                    allocations: 0,
                    bytes: 0,
                },
            }),
        }
    }

    fn lock(&self) -> Lock<'_> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        Lock(&self.locked)
    }

    pub fn leak_report(&self) -> LeakReport {
        let _lock = self.lock();
        unsafe { (*self.state.get()).leaks }
    }

    pub fn check(&self) -> Result<LeakReport, CheckError> {
        let _lock = self.lock();
        unsafe {
            let state = &*self.state.get();
            for i in 0..state.capacity {
                let entry = *state.table.add(i);
                if !entry.record.is_null() {
                    self.check_record(entry.record)?;
                }
            }
            Ok(state.leaks)
        }
    }

    pub fn deinit(self) -> LeakReport {
        let report = self.leak_report();
        drop(self);
        report
    }

    unsafe fn check_record(&self, record: *mut Record) -> Result<(), CheckError> {
        unsafe {
            let r = &*record;
            let address = r.user.addr();
            if r.checksum != checksum(r, self.options.canary) {
                return Err(CheckError::MetadataCorrupted { address });
            }
            let guard = guard(r, self.options.canary);
            if r.user.sub(GUARD_SIZE).cast::<u64>().read_unaligned() != guard
                || r.user.add(r.size).cast::<u64>().read_unaligned() != guard
            {
                return Err(CheckError::GuardCorrupted { address });
            }
            if r.freed != 0 && self.options.check_write_after_free {
                for i in 0..r.size {
                    if r.user.add(i).read() != 0x55 {
                        return Err(CheckError::FreedMemoryModified {
                            address: address + i,
                        });
                    }
                }
            }
            Ok(())
        }
    }

    unsafe fn reserve_entry(&self, state: &mut State) -> Result<(), AllocError> {
        if state.capacity != 0 && state.used < state.capacity / 2 {
            return Ok(());
        }
        let capacity = if state.capacity == 0 {
            64
        } else if state.occupied < state.capacity / 4 {
            state.capacity
        } else {
            state.capacity.checked_mul(2).ok_or(AllocError)?
        };
        let layout = Layout::array::<Entry>(capacity).map_err(|_| AllocError)?;
        let table = self.parent.allocate(layout)?.cast::<Entry>().as_ptr();
        unsafe {
            for i in 0..capacity {
                table.add(i).write(Entry {
                    address: 0,
                    record: ptr::null_mut(),
                });
            }
            for i in 0..state.capacity {
                let entry = *state.table.add(i);
                if !entry.record.is_null() {
                    let mut at = hash(entry.address) & (capacity - 1);
                    while !(*table.add(at)).record.is_null() {
                        at = (at + 1) & (capacity - 1);
                    }
                    table.add(at).write(entry);
                }
            }
            if state.capacity != 0 {
                self.parent.deallocate(
                    NonNull::new_unchecked(state.table.cast()),
                    Layout::array::<Entry>(state.capacity).unwrap_or_else(|_| corrupt()),
                );
            }
        }
        state.table = table;
        state.capacity = capacity;
        state.used = state.occupied;
        Ok(())
    }

    unsafe fn release_bucket(&self, state: &mut State, bucket: *mut Bucket) {
        unsafe {
            let mut record = (*bucket).first;
            while !record.is_null() {
                if self.check_record(record).is_err() {
                    corrupt();
                }
                let slot = find(state, (*record).user.addr()).unwrap_or_else(|| corrupt());
                (*state.table.add(slot)).record = ptr::null_mut();
                (*state.table.add(slot)).address = 1;
                state.occupied -= 1;
                record = (*record).next;
            }
            let next = (*bucket).next;
            let prev = (*bucket).prev;
            if prev.is_null() {
                state.first = next;
            } else {
                (*prev).next = next;
            }
            if !next.is_null() {
                (*next).prev = prev;
            }
            self.parent
                .deallocate(NonNull::new_unchecked(bucket.cast()), (*bucket).layout);
        }
    }
}

unsafe impl<A: Allocator> Allocator for SafeAllocator<A> {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let _lock = self.lock();
        unsafe {
            let state = &mut *self.state.get();
            let (_, _, required) = placement(size_of::<Bucket>(), layout)?;
            let large = required >= BUCKET_SIZE * 3 / 4 || layout.align() > BUCKET_SIZE;
            let backing_layout = Layout::from_size_align(
                if large { required } else { BUCKET_SIZE },
                if large {
                    layout
                        .align()
                        .max(align_of::<Bucket>())
                        .max(align_of::<Record>())
                } else {
                    BUCKET_SIZE
                },
            )
            .map_err(|_| AllocError)?;
            let allocations = state.leaks.allocations.checked_add(1).ok_or(AllocError)?;
            let bytes = state
                .leaks
                .bytes
                .checked_add(layout.size())
                .ok_or(AllocError)?;
            self.reserve_entry(state)?;
            let mut bucket = if large {
                ptr::null_mut()
            } else {
                state.current
            };
            if !bucket.is_null()
                && (layout.align() > (*bucket).layout.align()
                    || placement((*bucket).fill, layout)?.2 > (*bucket).layout.size())
            {
                state.current = ptr::null_mut();
                if (*bucket).live == 0 {
                    self.release_bucket(state, bucket);
                }
                bucket = ptr::null_mut();
            }
            if bucket.is_null() {
                bucket = self
                    .parent
                    .allocate(backing_layout)?
                    .cast::<Bucket>()
                    .as_ptr();
                bucket.write(Bucket {
                    next: state.first,
                    prev: ptr::null_mut(),
                    layout: backing_layout,
                    fill: size_of::<Bucket>(),
                    live: 0,
                    first: ptr::null_mut(),
                });
                if !state.first.is_null() {
                    (*state.first).prev = bucket;
                }
                state.first = bucket;
                if !large {
                    state.current = bucket;
                }
            }
            let (record_at, user_at, end) = placement((*bucket).fill, layout)?;
            let base = bucket.cast::<u8>();
            let record = base.add(record_at).cast::<Record>();
            let user = base.add(user_at);
            record.write(Record {
                next: (*bucket).first,
                bucket,
                user,
                size: layout.size(),
                align: layout.align(),
                freed: 0,
                checksum: 0,
            });
            (*record).checksum = checksum(&*record, self.options.canary);
            let guard = guard(&*record, self.options.canary);
            user.sub(GUARD_SIZE).cast::<u64>().write_unaligned(guard);
            user.add(layout.size()).cast::<u64>().write_unaligned(guard);
            (*bucket).first = record;
            (*bucket).fill = end;
            (*bucket).live += 1;
            let mut at = hash(user.addr()) & (state.capacity - 1);
            while !(*state.table.add(at)).record.is_null() {
                at = (at + 1) & (state.capacity - 1);
            }
            state.used += usize::from((*state.table.add(at)).address == 0);
            state.occupied += 1;
            state.table.add(at).write(Entry {
                address: user.addr(),
                record,
            });
            state.leaks = LeakReport { allocations, bytes };
            Ok(NonNull::slice_from_raw_parts(
                NonNull::new_unchecked(user),
                layout.size(),
            ))
        }
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        let _lock = self.lock();
        unsafe {
            let state = &mut *self.state.get();
            let at = find(state, ptr.addr().get()).unwrap_or_else(|| corrupt());
            let record = (*state.table.add(at)).record;
            if self.check_record(record).is_err() {
                corrupt();
            }
            let r = &mut *record;
            if r.freed != 0 || r.size != layout.size() || r.align != layout.align() {
                corrupt();
            }
            if self.options.check_write_after_free {
                r.user.write_bytes(0x55, r.size);
            }
            r.freed = 1;
            r.checksum = checksum(r, self.options.canary);
            let bucket = r.bucket;
            (*bucket).live -= 1;
            state.leaks.allocations -= 1;
            state.leaks.bytes -= layout.size();
            if (*bucket).live == 0 && bucket != state.current {
                self.release_bucket(state, bucket);
            }
        }
    }
}

impl<A: Allocator> Drop for SafeAllocator<A> {
    fn drop(&mut self) {
        unsafe {
            let state = &mut *self.state.get();
            while !state.first.is_null() {
                self.release_bucket(state, state.first);
            }
            if state.capacity != 0 {
                self.parent.deallocate(
                    NonNull::new_unchecked(state.table.cast()),
                    Layout::array::<Entry>(state.capacity).unwrap_or_else(|_| corrupt()),
                );
            }
        }
    }
}

fn placement(fill: usize, layout: Layout) -> Result<(usize, usize, usize), AllocError> {
    let record = align_up(fill, align_of::<Record>())?;
    let before = record
        .checked_add(size_of::<Record>() + GUARD_SIZE)
        .ok_or(AllocError)?;
    let user = align_up(before, layout.align())?;
    let end = user
        .checked_add(layout.size())
        .and_then(|v| v.checked_add(GUARD_SIZE))
        .ok_or(AllocError)?;
    Ok((record, user, end))
}

fn align_up(value: usize, alignment: usize) -> Result<usize, AllocError> {
    value
        .checked_add(alignment - 1)
        .map(|v| v & !(alignment - 1))
        .ok_or(AllocError)
}

fn hash(address: usize) -> usize {
    let x = address.bitxor(address >> 16);
    x.wrapping_mul(0x9e37_79b9).bitxor(x >> 8)
}

fn checksum(r: &Record, canary: u32) -> u64 {
    let mut result = guard(r, canary);
    for value in [r.next.addr(), r.bucket.addr(), usize::from(r.freed)] {
        result = result
            .rotate_left(17)
            .bitxor((value as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
    }
    result
}

fn guard(r: &Record, canary: u32) -> u64 {
    u64::from(canary)
        .bitxor((r.user.addr() as u64).rotate_left(13))
        .bitxor((r.size as u64).rotate_left(29))
        .bitxor(r.align as u64)
}

unsafe fn find(state: &State, address: usize) -> Option<usize> {
    if state.capacity == 0 {
        return None;
    }
    let mut at = hash(address) & (state.capacity - 1);
    unsafe {
        loop {
            let entry = *state.table.add(at);
            if !entry.record.is_null() && entry.address == address {
                return Some(at);
            }
            if entry.record.is_null() && entry.address == 0 {
                return None;
            }
            at = (at + 1) & (state.capacity - 1);
        }
    }
}

#[cold]
// Corruption not bad. Corruption only bad when I'm not involved.
extern "C" fn corrupt() -> ! {
    panic!("SafeAllocator detected invalid allocator use or memory corruption")
}
