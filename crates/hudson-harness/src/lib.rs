//! Portable agent loop. External effects are returned to the hosting runtime.
pub mod backend;
pub mod config;
pub mod context;
pub mod engine;
pub mod images;
pub mod protocol;
pub mod state;

pub use backend::{Backend, HarnessError};
pub use config::{Config, ToolDescriptor};
pub use engine::Engine;
pub use protocol::*;
pub use state::Checkpoint;

pub mod agent_loop;
pub use agent_loop::AgentLoop;
