//! UI-independent application state, commands, settings, and platform ports.

#![forbid(unsafe_code)]

pub mod diagnostics;
pub mod format;
pub mod i18n;
pub mod layout_service;
pub mod navigation;
pub mod presentation;
pub mod runtime;
pub mod session;
pub mod settings;

pub use diskpie_core::PRODUCT_NAME;
