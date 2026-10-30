use super::*;
use hudson_harness::{AgentLoop, Config, Role, ToolCall, ToolResult};

fn workload() -> (Engine<AgentLoop>, Config, Checkpoint, Input, ContextPolicy) {
    workload_with_exchanges(120)
}

fn workload_with_exchanges(
    count: usize,
) -> (Engine<AgentLoop>, Config, Checkpoint, Input, ContextPolicy) {
    let engine = Engine::new(AgentLoop);
    let config = Config {
        instructions: "Keep exact evidence".into(),
        model: "fixture".into(),
        tools: vec![],
        allow_user_input: false,
        max_context_bytes: 16_384,
    };
    let started = engine
        .advance(
            &config,
            &engine.initial_state(),
            Input::Start {
                value: json!("original task"),
            },
        )
        .unwrap();
    let mut history = agent_loop::history(&started.checkpoint).unwrap();
    for index in 0..count {
        let id = format!("call-{index}");
        history.messages.push(Message {
            role: Role::Assistant,
            content: vec![Content::ToolCall {
                call: ToolCall {
                    call_id: id.clone(),
                    name: "read".into(),
                    arguments: json!({}),
                    provider_metadata: json!({"signature":"provider-evidence"}),
                },
            }],
        });
        history.messages.push(Message {
            role: Role::Tool,
            content: vec![Content::ToolResult {
                result: ToolResult {
                    call_id: id,
                    outcome: ToolOutcome::Success {
                        value: json!("x".repeat(512)),
                    },
                },
            }],
        });
    }
    let checkpoint = agent_loop::with_history(&started.checkpoint, history).unwrap();
    let input = Input::Model {
        response: hudson_harness::ModelResponse::Final {
            output: json!("answer"),
        },
    };
    let policy = serde_json::from_value(
        json!({"offload_bytes":4096,"recent_exchanges":2,"excerpt_bytes":256,"page_bytes":512}),
    )
    .unwrap();
    (engine, config, checkpoint, input, policy)
}

#[test]
fn explicit_compaction_archives_fitting_history_without_changing_the_action() {
    let (engine, mut config, checkpoint, input, policy) = workload();
    config.max_context_bytes = 256 * 1024;
    let automatic = advance(
        &engine,
        &config,
        &checkpoint,
        input.clone(),
        Control {
            policy: Some(&policy),
            compact: false,
        },
        "workspace",
        Uuid::nil(),
    )
    .unwrap();
    assert!(automatic.artifacts.is_empty());
    let prepared = advance(
        &engine,
        &config,
        &checkpoint,
        input,
        Control {
            policy: Some(&policy),
            compact: true,
        },
        "workspace",
        Uuid::nil(),
    )
    .unwrap();
    assert_eq!(automatic.transition.action, prepared.transition.action);
    let report = prepared.compaction.as_ref().unwrap();
    assert_eq!(report.archived_exchanges, 118);
    assert!(report.history_bytes_after < report.history_bytes_before / 10);
    assert_eq!(prepared.artifacts.len(), 118);
    let history = agent_loop::history(&prepared.transition.checkpoint).unwrap();
    assert_eq!(
        history.messages[0].content,
        vec![Content::Json {
            value: json!("original task")
        }]
    );
    let last = prepared.artifacts.last().unwrap();
    assert_eq!(report.artifact_id.as_ref(), Some(&last.id));
    for pair in prepared.artifacts.windows(2) {
        assert_eq!(pair[1].content["previous_artifact"], pair[0].id);
    }
}

#[test]
fn explicit_compaction_bounds_work_and_preserves_images_directions_and_pending_calls() {
    let (_, _, checkpoint, _, mut policy) = workload_with_exchanges(260);
    policy.recent_exchanges = 1;
    let mut history = agent_loop::history(&checkpoint).unwrap();
    let direction = Message {
        role: Role::User,
        content: vec![
            Content::Json {
                value: json!("latest direction"),
            },
            Content::Image {
                image: hudson_harness::images::Image {
                    media_type: "image/png".into(),
                    data: "YQ==".into(),
                },
            },
        ],
    };
    let pending = Message {
        role: Role::Assistant,
        content: vec![Content::ToolCall {
            call: ToolCall {
                call_id: "pending".into(),
                name: "read".into(),
                arguments: json!({}),
                provider_metadata: json!({}),
            },
        }],
    };
    history
        .messages
        .extend([direction.clone(), pending.clone()]);
    let mut artifacts = Vec::new();
    let report = compact_history(
        &mut history,
        &mut artifacts,
        "workspace",
        Uuid::nil(),
        &policy,
    )
    .unwrap();
    assert_eq!(report.archived_exchanges, 128);
    assert_eq!(artifacts.len(), 128);
    assert!(history.messages.contains(&direction));
    assert!(history.messages.contains(&pending));
    for artifact in artifacts {
        let messages: Vec<Message> =
            serde_json::from_value(artifact.content["messages"].clone()).unwrap();
        assert!(matches!(
            messages.as_slice(),
            [
                Message {
                    role: Role::Assistant,
                    ..
                },
                Message {
                    role: Role::Tool,
                    ..
                }
            ]
        ));
    }
}

#[test]
fn compaction_preserves_recent_exchanges_originals_and_advisory_headroom() {
    let (engine, config, checkpoint, input, mut policy) = workload();
    policy.reserve_bytes = 4096;
    let prepared = advance(
        &engine,
        &config,
        &checkpoint,
        input,
        Control {
            policy: Some(&policy),
            compact: false,
        },
        "workspace",
        Uuid::nil(),
    )
    .unwrap();
    assert!(
        serde_json::to_vec(&prepared.transition.checkpoint)
            .unwrap()
            .len()
            <= config.max_context_bytes - policy.reserve_bytes
    );
    let history = agent_loop::history(&prepared.transition.checkpoint).unwrap();
    assert_eq!(
        history.messages[0].content,
        vec![Content::Json {
            value: json!("original task")
        }]
    );
    assert!(history.messages.iter().any(|message| message
        .content
        .iter()
        .any(|content| matches!(content,
        Content::ToolCall { call } if call.call_id == "call-119"))));
    assert!(!prepared.artifacts.is_empty());
    for artifact in &prepared.artifacts {
        let messages: Vec<Message> =
            serde_json::from_value(artifact.content["messages"].clone()).unwrap();
        let Content::ToolCall { call } = &messages[0].content[0] else {
            panic!()
        };
        let Content::ToolResult { result } = &messages[1].content[0] else {
            panic!()
        };
        assert_eq!(call.call_id, result.call_id);
        assert_eq!(call.provider_metadata["signature"], "provider-evidence");
    }
}

#[test]
fn default_reserve_preserves_existing_policy_fingerprints() {
    let policy: ContextPolicy = serde_json::from_value(
        json!({"offload_bytes":8192,"recent_exchanges":2,"excerpt_bytes":1024,"page_bytes":1024}),
    )
    .unwrap();
    assert_eq!(policy.reserve_bytes, 0);
    assert!(serde_json::to_value(policy)
        .unwrap()
        .get("reserve_bytes")
        .is_none());
}

#[test]
fn reserve_does_not_reject_a_valid_minimal_context() {
    let engine = Engine::new(AgentLoop);
    let config = Config {
        instructions: "finish".into(),
        model: "fixture".into(),
        tools: vec![],
        allow_user_input: false,
        max_context_bytes: 4096,
    };
    let policy = ContextPolicy {
        reserve_bytes: 4000,
        ..Default::default()
    };
    let prepared = advance(
        &engine,
        &config,
        &engine.initial_state(),
        Input::Start {
            value: json!("small original task"),
        },
        Control {
            policy: Some(&policy),
            compact: false,
        },
        "workspace",
        Uuid::nil(),
    )
    .unwrap();
    assert!(
        serde_json::to_vec(&prepared.transition.checkpoint)
            .unwrap()
            .len()
            <= config.max_context_bytes
    );
    assert!(prepared.artifacts.is_empty());
}

/// Fixed local workload; assertions cover correctness, timing is reported only.
#[test]
#[ignore = "reproducible context-compaction workload; use --ignored --nocapture"]
fn compaction_workload() {
    let (engine, config, checkpoint, input, policy) = workload();
    let mut timings = Vec::new();
    for _ in 0..5 {
        let started = std::time::Instant::now();
        let prepared = advance(
            &engine,
            &config,
            &checkpoint,
            input.clone(),
            Control {
                policy: Some(&policy),
                compact: false,
            },
            "workspace",
            Uuid::nil(),
        )
        .unwrap();
        timings.push(started.elapsed().as_micros());
        assert!(
            serde_json::to_vec(&prepared.transition.checkpoint)
                .unwrap()
                .len()
                <= config.max_context_bytes
        );
        assert!(prepared.artifacts.len() > 80);
    }
    timings.sort_unstable();
    println!(
        "120 complete exchanges, 512-byte results, 16 KiB checkpoint: median {} us, samples {:?}",
        timings[2], timings
    );
}
