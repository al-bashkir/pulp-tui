//! Pulp REST API layer: transport (`client`), capability discovery (`caps`),
//! typed models (`models`), plugin-independent operations (`ops`) and per-plugin
//! operations (`rpm`, `file`).

pub mod caps;
pub mod client;
pub mod file;
pub mod models;
pub mod ops;
pub mod rpm;

pub use caps::{Capabilities, Feature};
pub use client::{Mutation, PulpClient, PulpError};
pub use models::RepoType;
