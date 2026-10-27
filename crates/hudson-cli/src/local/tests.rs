use super::*;
use hudson_core::adapters::tools::{ExecutionError, Invocation, ToolExecutor};
use hudson_harness::{ModelRequest, ModelResponse, ToolCall};
use std::collections::VecDeque;

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("hudson-local-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct Script {
    responses: VecDeque<ModelResponse>,
    follow_up: bool,
}
impl ModelExecutor for Script {
    fn call(&mut self, request: &ModelRequest) -> Result<ModelResponse, ExecutionError> {
        if self.follow_up {
            let text = serde_json::to_string(&request.messages).unwrap();
            assert!(text.contains("Fix the calculation"));
            assert!(text.contains("fixed and verified"));
            assert!(text.contains("Now explain"));
        }
        Ok(self.responses.pop_front().expect("unexpected model call"))
    }
}
fn tool(name: &str, arguments: serde_json::Value) -> ModelResponse {
    ModelResponse::ToolCalls {
        calls: vec![ToolCall {
            call_id: Uuid::new_v4().to_string(),
            name: name.into(),
            arguments,
            provider_metadata: serde_json::Value::Null,
        }],
    }
}
fn host(
    root: &Path,
    directory: &Path,
    permission: Permission,
    responses: Vec<ModelResponse>,
) -> Host {
    fs::create_dir_all(directory).unwrap();
    let session = Session {
        schema_version: 1,
        id: Uuid::new_v4(),
        workspace: root.into(),
        permission,
        model: "fixture".into(),
        endpoint: "http://127.0.0.1:1".into(),
        instructions: "Complete the task using tools.".into(),
        turns: vec![],
    };
    Host::build(
        session,
        directory.into(),
        Store::local_file(&directory.join("state.json")).unwrap(),
        Box::new(Script {
            responses: responses.into(),
            follow_up: false,
        }),
        mpsc::sync_channel(512).0,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap()
}
#[test]
fn local_tools_fix_a_fixture_and_follow_up_survives_reopen() {
    let root = Directory::new();
    let state = Directory::new();
    fs::write(
        root.0.join("calculation.py"),
        "value = 1\nassert value == 2\n",
    )
    .unwrap();
    let responses = vec![
        tool("read_file", json!({"path":"calculation.py"})),
        tool("shell", json!({"command":"python3 calculation.py"})),
        tool(
            "edit_file",
            json!({"path":"calculation.py","old_text":"value = 1","new_text":"value = 2"}),
        ),
        tool("shell", json!({"command":"python3 calculation.py"})),
        ModelResponse::Final {
            output: json!("fixed and verified"),
        },
    ];
    let mut host = host(&root.0, &state.0, Permission::FullAccess, responses);
    host.prompt("Fix the calculation".into()).unwrap();
    host.drive(&mpsc::channel().1).unwrap();
    let id = host.active().unwrap();
    let view = host.runtime.store.inspect(&host.actor, id).unwrap();
    assert_eq!(view.status, RunStatus::Completed);
    assert_eq!(view.usage.tool_calls, 4);
    let operations = host.runtime.store.operations(&host.actor, id).unwrap();
    let results: Vec<_> = operations
        .iter()
        .filter_map(|op| match &op.result {
            Some(OperationResult::Tool { result }) => {
                Some(serde_json::to_string(&result.outcome).unwrap())
            }
            _ => None,
        })
        .collect();
    assert!(results.iter().any(|r| r.contains("\"success\":false")));
    assert!(results.iter().any(|r| r.contains("\"success\":true")));
    assert!(fs::read_to_string(root.0.join("calculation.py"))
        .unwrap()
        .contains("value = 2"));
    let session: Session =
        serde_json::from_slice(&fs::read(state.0.join("session.json")).unwrap()).unwrap();
    drop(host);
    let mut reopened = Host::build(
        session,
        state.0.clone(),
        Store::local_file(&state.0.join("state.json")).unwrap(),
        Box::new(Script {
            responses: vec![ModelResponse::Final {
                output: json!("Explanation"),
            }]
            .into(),
            follow_up: true,
        }),
        mpsc::sync_channel(512).0,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    reopened.prompt("Now explain".into()).unwrap();
    reopened.drive(&mpsc::channel().1).unwrap();
    assert_eq!(
        reopened
            .runtime
            .store
            .inspect(&reopened.actor, reopened.active().unwrap())
            .unwrap()
            .status,
        RunStatus::Completed
    );
    assert_eq!(
        reopened
            .runtime
            .store
            .inspect(&reopened.actor, id)
            .unwrap()
            .usage
            .tool_calls,
        4
    );
    let foreign = Actor {
        workspace_id: reopened.actor.workspace_id.clone(),
        id: "someone-else".into(),
    };
    assert!(reopened
        .runtime
        .submit_turn(
            &foreign,
            reopened.reference.clone(),
            json!("steal context"),
            None,
            id
        )
        .is_err());
    let key = Some("repeat-turn".into());
    let first = reopened
        .runtime
        .submit_turn(
            &reopened.actor,
            reopened.reference.clone(),
            json!("continue"),
            key.clone(),
            id,
        )
        .unwrap();
    assert_eq!(
        first,
        reopened
            .runtime
            .submit_turn(
                &reopened.actor,
                reopened.reference.clone(),
                json!("continue"),
                key,
                id
            )
            .unwrap()
    );
}

#[test]
#[cfg(unix)]
fn an_uncertain_shell_write_is_not_replayed_after_reopen() {
    let root = Directory::new();
    let state = Directory::new();
    let mut host = host(
        &root.0,
        &state.0,
        Permission::FullAccess,
        vec![tool(
            "shell",
            json!({"command":"printf x >> count; sleep 3","timeout_seconds":1}),
        )],
    );
    host.prompt("Do bounded work".into()).unwrap();
    host.drive(&mpsc::channel().1).unwrap();
    assert_eq!(
        host.runtime
            .store
            .inspect(&host.actor, host.active().unwrap())
            .unwrap()
            .status,
        RunStatus::Waiting
    );
    let session: Session =
        serde_json::from_slice(&fs::read(state.0.join("session.json")).unwrap()).unwrap();
    drop(host);
    let mut reopened = Host::build(
        session,
        state.0.clone(),
        Store::local_file(&state.0.join("state.json")).unwrap(),
        Box::new(Script {
            responses: VecDeque::new(),
            follow_up: false,
        }),
        mpsc::sync_channel(512).0,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    reopened.drive(&mpsc::channel().1).unwrap();
    assert_eq!(fs::read_to_string(root.0.join("count")).unwrap(), "x");
}
#[test]
fn approval_precedes_actual_file_mutation() {
    let root = Directory::new();
    let state = Directory::new();
    let mut host = host(
        &root.0,
        &state.0,
        Permission::Ask,
        vec![
            tool("write_file", json!({"path":"new.txt","content":"approved"})),
            ModelResponse::Final {
                output: json!("done"),
            },
        ],
    );
    host.prompt("Create a file".into()).unwrap();
    host.drive(&mpsc::channel().1).unwrap();
    assert!(!root.0.join("new.txt").exists());
    let id = host.active().unwrap();
    let view = host.runtime.store.inspect(&host.actor, id).unwrap();
    let Some(WaitReason::Approval { operation_id }) = view.wait else {
        panic!("approval required")
    };
    host.runtime
        .approve(&host.actor, operation_id, true, now(), now() + 60_000)
        .unwrap();
    host.drive(&mpsc::channel().1).unwrap();
    assert_eq!(
        fs::read_to_string(root.0.join("new.txt")).unwrap(),
        "approved"
    );
}
#[test]
fn file_scope_conflicts_and_uncertain_shell_effects_are_preserved() {
    let root = Directory::new();
    let outside = Directory::new();
    let (events, _receiver) = mpsc::sync_channel(512);
    let mut tools = tools::LocalTools {
        root: root.0.clone(),
        events,
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let actor = Actor {
        workspace_id: "test".into(),
        id: "test".into(),
    };
    let definitions = tools::definitions(&actor, Permission::FullAccess);
    let mut call = |name: &str, arguments: serde_json::Value| {
        tools.execute(Invocation {
            operation_id: Uuid::new_v4(),
            tool: definitions.iter().find(|t| t.name == name).unwrap(),
            arguments: &arguments,
        })
    };
    assert!(call("read_file", json!({"path":outside.0.join("secret")})).is_err());
    fs::write(root.0.join("a"), "user edits").unwrap();
    assert!(call(
        "write_file",
        json!({"path":"a","content":"clobber","expected_digest":"stale"})
    )
    .is_err());
    assert_eq!(fs::read_to_string(root.0.join("a")).unwrap(), "user edits");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside.0, root.0.join("escape")).unwrap();
        assert!(call("write_file", json!({"path":"escape/new","content":"no"})).is_err());
        let value = call(
            "shell",
            json!({"command":"printf done > partial; sleep 5","timeout_seconds":1}),
        );
        assert!(matches!(value, Err(ExecutionError::Unknown(_))));
        assert_eq!(fs::read_to_string(root.0.join("partial")).unwrap(), "done");
    }
    let readonly = tools::definitions(&actor, Permission::ReadOnly);
    assert!(readonly.iter().all(|t| t.effect == Effect::Read));
}
