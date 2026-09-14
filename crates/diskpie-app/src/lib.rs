//! UI-independent application state, commands, settings, and platform ports.

#![forbid(unsafe_code)]

pub mod actions;
pub mod branch_rescan;
pub mod diagnostics;
pub mod explorer_integration;
pub mod format;
pub mod i18n;
pub mod item_list_service;
pub mod layout_service;
pub mod navigation;
pub mod presentation;
pub mod runtime;
pub mod session;
pub mod settings;

pub use diskpie_core::PRODUCT_NAME;
