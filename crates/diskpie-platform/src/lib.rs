//! Filesystem and operating-system adapters for DiskPie.
//!
//! Project-authored unsafe code is permitted only in narrowly reviewed modules
//! under this crate's Windows adapter. Every unsafe block must document its
//! invariants and translate raw resources into RAII wrappers immediately.

#![deny(unsafe_op_in_unsafe_fn)]

pub use diskpie_core::PRODUCT_NAME;
