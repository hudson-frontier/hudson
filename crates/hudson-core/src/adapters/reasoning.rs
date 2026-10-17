//! Explicit, provider-specific thinking controls; output limits remain authoritative.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn provider_modes_are_explicit_and_budgets_leave_output_room() {
        let effort = Reasoning::Effort {
            effort: Effort::Low,
        };
        for provider in ["openai", "gemini", "anthropic"] {
            effort.validate(provider, 4096).unwrap();
        }
        assert!(effort.validate("ollama", 4096).is_err());
        for tokens in [0, 1023, 4096, u32::MAX] {
            assert!(Reasoning::Budget { tokens }
                .validate("anthropic", 4096)
                .is_err());
        }
        Reasoning::Budget { tokens: 1024 }
            .validate("anthropic", 4096)
            .unwrap();
        assert!(Reasoning::Budget { tokens: 1024 }
            .validate("openai", 4096)
            .is_err());
        assert!(Reasoning::Effort {
            effort: Effort::Max
        }
        .validate("gemini", 4096)
        .is_err());
        assert!(serde_json::from_value::<Reasoning>(
            json!({"type":"effort","effort":"low","unexpected":true})
        )
        .is_err());
    }
    #[test]
    fn explicit_controls_preserve_the_overall_output_limit() {
        for (reasoning, native, expected) in [
            (
                Reasoning::Effort {
                    effort: Effort::Medium,
                },
                false,
                json!({"max_tokens":4096,"reasoning_effort":"medium"}),
            ),
            (
                Reasoning::Effort {
                    effort: Effort::High,
                },
                true,
                json!({"max_tokens":4096,"thinking":{"type":"adaptive"},"output_config":{"effort":"high"}}),
            ),
            (
                Reasoning::Budget { tokens: 1024 },
                true,
                json!({"max_tokens":4096,"thinking":{"type":"enabled","budget_tokens":1024}}),
            ),
        ] {
            let mut body = json!({"max_tokens":4096});
            reasoning.apply(&mut body, native);
            assert_eq!(body, expected);
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reasoning {
    /// Chat reasoning_effort, or adaptive native thinking with output_config.effort.
    Effort { effort: Effort },
    /// Manual native thinking for models that support budget_tokens.
    Budget { tokens: u32 },
}

impl Reasoning {
    pub fn validate(&self, provider: &str, max_output_tokens: u32) -> Result<()> {
        let valid = match (provider, self) {
            ("" | "openai", Self::Effort { .. }) => true,
            ("gemini", Self::Effort { effort }) => !matches!(effort, Effort::Xhigh | Effort::Max),
            ("anthropic", Self::Effort { effort }) => {
                !matches!(effort, Effort::None | Effort::Minimal)
            }
            ("anthropic", Self::Budget { tokens }) => {
                *tokens >= 1024 && *tokens < max_output_tokens
            }
            _ => false,
        };
        if valid {
            Ok(())
        } else {
            Err(Error::Invalid(
                "reasoning mode is unsupported for this provider or exceeds the output budget"
                    .into(),
            ))
        }
    }

    pub(super) fn apply(&self, body: &mut Value, anthropic: bool) {
        match self {
            Self::Effort { effort } if anthropic => {
                body["thinking"] = json!({"type":"adaptive"});
                body["output_config"] = json!({"effort":effort});
            }
            Self::Effort { effort } => body["reasoning_effort"] = json!(effort),
            Self::Budget { tokens } => {
                body["thinking"] = json!({"type":"enabled","budget_tokens":tokens})
            }
        }
    }
}
