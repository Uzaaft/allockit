//! Composable allocators for Rust's [`Allocator`](core::alloc::Allocator) trait.

#![no_std]

#[cfg(feature = "std")]
extern crate std;

#[cfg(feature = "buffer-first-allocator")]
pub use buffer_first_allocator::BufferFirstAllocator;

#[cfg(feature = "failing-allocator")]
pub use failing_allocator::{Config, FailingAllocator};
