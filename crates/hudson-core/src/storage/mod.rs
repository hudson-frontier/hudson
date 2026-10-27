//! Atomic state storage. External effects must occur outside transactions.
mod local;
pub mod memory;
mod pairs;
mod postgres;
pub use memory::MemoryStore;
/// Preferred name for a handle backed by memory or PostgreSQL.
pub type Store = MemoryStore;
