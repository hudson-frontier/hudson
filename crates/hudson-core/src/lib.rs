//! Agent execution, model and tool adapters, and runtime controls.
pub mod adapters;
pub mod budgets;
pub mod credentials;
pub mod customer;
pub mod definitions;
pub mod dispatch;
pub mod models;
pub mod publication;
pub mod published_worker;
pub mod recovery;
pub mod runtime;
pub mod security;
pub mod storage;
pub mod verification;

#[cfg(feature = "fixtures")]
pub mod fixtures;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found")]
    NotFound,
    #[error("access denied")]
    Denied,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error(transparent)]
    Harness(#[from] hudson_harness::HarnessError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
pub type Result<T> = std::result::Result<T, Error>;

pub mod skills;

pub mod subagents;

pub mod configured;

pub mod evaluation;

pub mod context;
pub mod memory;

pub mod coordination;

pub mod scheduling;

pub mod sandbox_binding;
