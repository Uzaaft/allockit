//! Composable allocators for Rust's [`Allocator`](core::alloc::Allocator) trait.

#![no_std]

#[cfg(feature = "std")]
extern crate std;

#[cfg(feature = "arena-allocator")]
pub use arena_allocator::{ArenaAllocator, ResetMode};

#[cfg(feature = "buffer-first-allocator")]
pub use buffer_first_allocator::BufferFirstAllocator;

#[cfg(feature = "failing-allocator")]
pub use failing_allocator::{Config, FailingAllocator};

#[cfg(all(
    feature = "page-allocator",
    any(target_os = "macos", target_os = "linux")
))]
pub use page_allocator::PageAllocator;
