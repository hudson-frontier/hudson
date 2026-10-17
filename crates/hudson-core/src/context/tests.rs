use super::*;
use hudson_harness::{AgentLoop, Config, Role, ToolCall, ToolResult};

fn workload() -> (Engine<AgentLoop>, Config, Checkpoint, Input, ContextPolicy) {
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
    for index in 0..120 {
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
fn compaction_preserves_recent_exchanges_originals_and_advisory_headroom() {
    let (engine, config, checkpoint, input, mut policy) = workload();
    policy.reserve_bytes = 4096;
    let prepared = advance(
        &engine,
        &config,
        &checkpoint,
        input,
        Some(&policy),
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
        Some(&policy),
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
            Some(&policy),
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
