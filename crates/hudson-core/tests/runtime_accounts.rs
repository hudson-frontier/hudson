#![cfg(unix)]
use hudson_core::{adapters::runtime_bridge::accounts::*, models::Actor};
use serde_json::json;
use std::time::Duration;

fn actor() -> Actor {
    Actor {
        workspace_id: "project".into(),
        id: "owner".into(),
    }
}
fn configuration(code: &str, lifetime: u64) -> (tempfile::TempDir, AccountConfig) {
    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("peer.py");
    std::fs::write(&script, code).unwrap();
    let config = serde_json::from_value(
        json!({"id":"local","runtime_id":"fixture","runtime_version":1,
        "actions":["status","login","renew","cancel_login","logout","models"],
        "lifetime_seconds":lifetime,"transport":{"type":"stdio","executable":"/usr/bin/python3",
        "args":["-I",script],"cwd":directory.path(),"env":[]}}),
    )
    .unwrap();
    (directory, config)
}
const PEER: &str = r#"
import json,sys
count=0
for line in sys.stdin:
    body=json.loads(line); count+=1
    assert body['actor']=={'workspace_id':'project','id':'owner'}
    if body['action']=='login':
        response={'type':'login','authorization_url':'https://auth.example.com/login?state=transient'}
    elif body['action']=='models': response={'type':'models','models':['fixture-model']}
    else: response={'type':'status','status':{'expected_version':'1','installed_version':str(count),
        'protocol_ready':True,'credential':'required','login':'none'}}
    result={key:body[key] for key in ['protocol_version','account_id','runtime_id','runtime_version','request_key']}
    result['response']=response
    print(json.dumps(result),flush=True)
"#;

#[tokio::test]
async fn actor_bound_sessions_replay_only_identical_actions_and_close_explicitly() {
    let (_directory, config) = configuration(PEER, 10);
    config.validate().unwrap();
    let session = AccountSession::open(config, actor()).unwrap();
    let wrong = Actor {
        id: "other".into(),
        ..actor()
    };
    assert!(session
        .action(&wrong, "login", AccountAction::Login)
        .await
        .is_err());
    let first = session
        .action(&actor(), "login", AccountAction::Login)
        .await
        .unwrap();
    assert!(
        first
            == session
                .action(&actor(), "login", AccountAction::Login)
                .await
                .unwrap()
    );
    assert!(session
        .action(&actor(), "login", AccountAction::Logout)
        .await
        .is_err());
    let status = session
        .action(&actor(), "status", AccountAction::Status)
        .await
        .unwrap();
    match status {
        AccountOutcome::Status { status } => {
            assert_eq!(status.installed_version.as_deref(), Some("2"))
        }
        _ => panic!("status expected"),
    }
    assert!(matches!(
        session
            .action(&actor(), "models", AccountAction::Models)
            .await
            .unwrap(),
        AccountOutcome::Models { .. }
    ));
    assert!(session.close(&wrong).is_err());
    session.close(&actor()).unwrap();
    tokio::task::yield_now().await;
    assert!(session
        .action(&actor(), "later", AccountAction::Status)
        .await
        .is_err());
}

#[tokio::test]
async fn expiry_and_invalid_output_close_the_session_without_retries_or_raw_diagnostics() {
    for code in [
        "import time;time.sleep(30)",
        "import sys;print('private-token');sys.stderr.write('private-token')",
        "print('x'*65537,flush=True)",
    ] {
        let (_directory, config) = configuration(code, 1);
        let session = AccountSession::open(config, actor()).unwrap();
        let before = std::time::Instant::now();
        let error = session
            .action(&actor(), "once", AccountAction::Status)
            .await
            .err()
            .unwrap();
        assert!(!error.to_string().contains("private-token"));
        assert!(before.elapsed() < Duration::from_secs(5));
        assert!(session
            .action(&actor(), "again", AccountAction::Status)
            .await
            .is_err());
    }
}

#[tokio::test]
async fn action_count_and_response_identity_are_bounded() {
    let (_directory, config) = configuration(PEER, 10);
    let session = AccountSession::open(config, actor()).unwrap();
    for index in 0..32 {
        session
            .action(&actor(), &format!("status-{index}"), AccountAction::Status)
            .await
            .unwrap();
    }
    assert!(session
        .action(&actor(), "excess", AccountAction::Status)
        .await
        .is_err());
    session
        .action(&actor(), "status-0", AccountAction::Status)
        .await
        .unwrap();
    let code = PEER.replace(
        "result['response']=response",
        "result['account_id']='another';result['response']=response",
    );
    let (_directory, config) = configuration(&code, 10);
    let session = AccountSession::open(config, actor()).unwrap();
    assert!(session
        .action(&actor(), "status", AccountAction::Status)
        .await
        .is_err());
}

#[tokio::test]
async fn ungranted_account_actions_fail_before_peer_io() {
    let (_directory, mut config) = configuration(PEER, 10);
    config.actions = vec![AccountAction::Status];
    config.optional_env = vec!["HUDSON_TEST_MISSING_OPTIONAL_ACCOUNT_KEY".into()];
    let session = AccountSession::open(config, actor()).unwrap();
    assert!(session
        .action(&actor(), "login", AccountAction::Login)
        .await
        .is_err());
    match session
        .action(&actor(), "status", AccountAction::Status)
        .await
        .unwrap()
    {
        AccountOutcome::Status { status } => {
            assert_eq!(status.installed_version.as_deref(), Some("1"))
        }
        _ => panic!("status expected"),
    }
}

#[tokio::test]
async fn an_idle_peer_exit_closes_the_control_session() {
    let (_directory, config) = configuration("pass", 10);
    let session = AccountSession::open(config, actor()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !session.expired() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(session
        .action(&actor(), "status", AccountAction::Status)
        .await
        .is_err());
}
