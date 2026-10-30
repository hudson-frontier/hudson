//! Typed initial image input; ordinary JSON input never gains image semantics.
use crate::models::Goal;
pub use hudson_harness::images::Image;
use serde_json::Value;

pub struct ImageTurn {
    pub input: Value,
    pub images: Vec<Image>,
    pub request_key: String,
    pub goal: Option<Goal>,
}
