use hudson_harness::*;
use serde_json::json;

fn config() -> Config {
    Config {
        instructions: "finish".into(),
        model: "fixture".into(),
        tools: vec![],
        allow_user_input: true,
        max_context_bytes: 16_384,
    }
}

#[test]
fn queued_messages_follow_the_finalized_input_and_verified_answer() {
    let engine = Engine::new(AgentLoop);
    let config = config();
    let start = engine
        .advance(
            &config,
            &engine.initial_state(),
            Input::Queued {
                input: Box::new(Input::Start {
                    value: json!("original"),
                }),
                messages: vec![json!("new direction")],
            },
        )
        .unwrap();
    let Action::CallModel { request } = start.action else {
        panic!()
    };
    assert_eq!(request.messages.len(), 2);
    assert_eq!(
        request.messages[1].content,
        vec![Content::Json {
            value: json!("new direction")
        }]
    );
    let candidate = engine
        .advance(
            &config,
            &start.checkpoint,
            Input::Model {
                response: ModelResponse::Final {
                    output: json!("first answer"),
                },
            },
        )
        .unwrap();
    let next = engine
        .advance(
            &config,
            &candidate.checkpoint,
            Input::Queued {
                input: Box::new(Input::Verification {
                    passed: true,
                    feedback: String::new(),
                }),
                messages: vec![json!("follow-up")],
            },
        )
        .unwrap();
    let Action::CallModel { request } = next.action else {
        panic!()
    };
    assert_eq!(request.messages[2].role, Role::Assistant);
    assert_eq!(
        request.messages[3].content,
        vec![Content::Json {
            value: json!("follow-up")
        }]
    );
    assert_eq!(next.checkpoint.step, candidate.checkpoint.step + 1);
}

#[test]
fn queue_cannot_interrupt_a_tool_batch_or_answer_a_question() {
    let engine = Engine::new(AgentLoop);
    let config = config();
    let start = engine
        .advance(
            &config,
            &engine.initial_state(),
            Input::Start {
                value: json!("task"),
            },
        )
        .unwrap();
    let call = ToolCall {
        call_id: "ask".into(),
        name: "ask_user".into(),
        arguments: json!({"prompt":"Which account?"}),
        provider_metadata: json!(null),
    };
    assert!(engine
        .advance(
            &config,
            &start.checkpoint,
            Input::Queued {
                input: Box::new(Input::Model {
                    response: ModelResponse::ToolCalls {
                        calls: vec![call.clone()]
                    }
                }),
                messages: vec![json!("do something else")],
            }
        )
        .is_err());
    let waiting = engine
        .advance(
            &config,
            &start.checkpoint,
            Input::Model {
                response: ModelResponse::ToolCalls { calls: vec![call] },
            },
        )
        .unwrap();
    assert!(engine
        .advance(
            &config,
            &waiting.checkpoint,
            Input::Queued {
                input: Box::new(Input::Start {
                    value: json!("not an answer")
                }),
                messages: vec![json!("do something else")],
            }
        )
        .is_err());
}

#[test]
fn queue_is_bounded_and_cannot_nest() {
    let engine = Engine::new(AgentLoop);
    for messages in [vec![], vec![json!("x"); 17]] {
        assert!(engine
            .advance(
                &config(),
                &engine.initial_state(),
                Input::Queued {
                    input: Box::new(Input::Start {
                        value: json!("task")
                    }),
                    messages,
                }
            )
            .is_err());
    }
    assert!(engine
        .advance(
            &config(),
            &engine.initial_state(),
            Input::Queued {
                input: Box::new(Input::Queued {
                    input: Box::new(Input::Start {
                        value: json!("task")
                    }),
                    messages: vec![json!("x")]
                }),
                messages: vec![json!("x")],
            }
        )
        .is_err());
}
