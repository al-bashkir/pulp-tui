//! Pulp REST API layer: transport (`client`), capability discovery (`caps`),
//! typed models (`models`) and plugin operations (`ops`).

pub mod caps;
pub mod client;
pub mod models;
pub mod ops;

pub use caps::{Capabilities, Feature};
pub use client::{Mutation, PulpClient, PulpError};
pub use models::RepoType;
