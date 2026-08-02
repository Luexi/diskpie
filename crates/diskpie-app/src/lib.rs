//! UI-independent application state, commands, settings, and platform ports.

#![forbid(unsafe_code)]

pub mod i18n;
pub mod navigation;
pub mod session;

pub use diskpie_core::PRODUCT_NAME;
