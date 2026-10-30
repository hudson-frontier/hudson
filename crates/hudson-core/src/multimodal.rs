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

pub(crate) fn validate_turn(images: &[Image]) -> crate::Result<()> {
    hudson_harness::images::validate_turn(images)
        .map_err(|_| crate::Error::Invalid("invalid image media type, encoding or limits".into()))
}
