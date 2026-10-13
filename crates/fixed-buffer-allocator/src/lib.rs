//! A bump allocator over a borrowed, fixed-size buffer.
//!
//! Modeled on Zig's `std.heap.FixedBufferAllocator`.

#![no_std]

#[cfg(feature = "std")]
extern crate std;
