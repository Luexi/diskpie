//! Portable domain model and sunburst geometry for DiskPie.

#![forbid(unsafe_code)]

pub mod model;
pub mod sunburst;

pub use model::{
    Aggregate, AggregateField, AggregateSize, Children, EntryKind, FileIdentity, GenerationId,
    HardLinkStatus, LinkField, MetricSource, ModelError, NodeId, NodeRecord, NodeSpec, OwnMetrics,
    ReparseKind, ScanState, SizeMetric, StructureError, TreeBuilder, TreeSnapshot, UnknownReason,
    VolumeKey,
};

/// Human-readable product name shared by every crate.
pub const PRODUCT_NAME: &str = "DiskPie";
