//! Composable allocators for Rust's [`Allocator`](core::alloc::Allocator) trait.

#![no_std]

#[cfg(feature = "alloc")]
extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

#[cfg(feature = "failing-allocator")]
pub use failing_allocator::FailingAllocator;
