use hudson_harness::{context::History, Content, Message, Role, ToolCall, ToolOutcome, ToolResult};
use serde_json::json;

fn result(id: &str) -> Message {
    Message {
        role: Role::Tool,
        content: vec![Content::ToolResult {
            result: ToolResult {
                call_id: id.into(),
                outcome: ToolOutcome::Success {
                    value: json!({"source":"unchanged"}),
                },
            },
        }],
    }
}
#[test]
fn archive_boundary_keeps_parallel_calls_and_provider_metadata_together() {
    let first = Message {
        role: Role::User,
        content: vec![Content::Text {
            text: "original objective".into(),
        }],
    };
    let batch = Message {
        role: Role::Assistant,
        content: ["one", "two"]
            .into_iter()
            .map(|id| Content::ToolCall {
                call: ToolCall {
                    call_id: id.into(),
                    name: "lookup".into(),
                    arguments: json!({}),
                    provider_metadata: json!({"signature":id}),
                },
            })
            .collect(),
    };
    let mut history = History {
        messages: vec![first.clone(), batch.clone(), result("one")],
        archive: None,
    };
    assert_eq!(history.oldest_complete_exchange(), None);
    history.messages.push(result("two"));
    assert_eq!(history.oldest_complete_exchange(), Some(1..4));
    let archived: Vec<_> = history.messages.drain(1..4).collect();
    assert_eq!(archived[0], batch);
    assert_eq!(history.messages, vec![first]);
}
#[test]
fn mismatched_results_never_become_a_compaction_boundary() {
    let mut history = History::default();
    history.messages.push(Message {
        role: Role::User,
        content: vec![],
    });
    history.messages.push(Message {
        role: Role::Assistant,
        content: vec![Content::ToolCall {
            call: ToolCall {
                call_id: "expected".into(),
                name: "tool".into(),
                arguments: json!({}),
                provider_metadata: json!(null),
            },
        }],
    });
    history.messages.push(result("different"));
    assert_eq!(history.oldest_complete_exchange(), None);
}

#[test]
fn newest_user_direction_survives_archiving_of_later_exchanges() {
    let user = |text: &str| Message {
        role: Role::User,
        content: vec![Content::Text { text: text.into() }],
    };
    let answer = Message {
        role: Role::Assistant,
        content: vec![Content::Json {
            value: json!("answer"),
        }],
    };
    let mut history = History {
        messages: vec![
            user("original"),
            user("old direction"),
            answer.clone(),
            user("current direction"),
            answer,
        ],
        archive: None,
    };
    assert_eq!(history.complete_exchanges(), vec![1..2, 2..3, 4..5]);
    while let Some(range) = history.oldest_complete_exchange() {
        history.messages.drain(range);
    }
    assert_eq!(
        history.messages,
        vec![user("original"), user("current direction")]
    );
}

#[test]
fn duplicate_call_ids_cannot_form_an_archive_boundary() {
    let call = Content::ToolCall {
        call: ToolCall {
            call_id: "same".into(),
            name: "read".into(),
            arguments: json!({}),
            provider_metadata: json!(null),
        },
    };
    let history = History {
        messages: vec![
            Message {
                role: Role::User,
                content: vec![],
            },
            Message {
                role: Role::Assistant,
                content: vec![call.clone(), call],
            },
            result("same"),
        ],
        archive: None,
    };
    assert!(history.oldest_complete_exchange().is_none());
    assert!(history.complete_exchanges().is_empty());
}
