//! Live SessionHub interop against the TypeScript Durable Object.
//!
//! ```sh
//! JOLT_EDGE_WS=ws://127.0.0.1:27640 \
//!   cargo test -p jolt-sync --test hub_edge -- --ignored
//! ```

use std::sync::Arc;
use std::time::Duration;

use jolt_session_doc::{
    SessionCommandStatus, TranscriptFrame, TranscriptManifest, TranscriptPage,
    TranscriptPageDescriptor,
};
use jolt_sync::{SessionHubClient, SessionHubEvent, StaticUrl};

#[tokio::test]
#[ignore = "requires a live edge: set JOLT_EDGE_WS (e.g. ws://127.0.0.1:27640)"]
async fn host_claim_resolve_and_projection_round_trip() {
    let ws_base = std::env::var("JOLT_EDGE_WS").expect("set JOLT_EDGE_WS");
    let http_base = ws_base
        .replacen("ws://", "http://", 1)
        .replacen("wss://", "https://", 1);
    let chat = format!("hub-{}", uuid::Uuid::new_v4().simple());
    let token = "alice@hub-integration";
    let url = format!("{ws_base}/hub/{chat}/ws?token={token}&role=host&device=device-a");
    let client = SessionHubClient::connect_via(Arc::new(StaticUrl(url)))
        .await
        .expect("connect host");
    let wrong_host_url =
        format!("{ws_base}/hub/{chat}/ws?token={token}&role=host&device=device-wrong");
    assert!(
        SessionHubClient::connect_via(Arc::new(StaticUrl(wrong_host_url)))
            .await
            .is_err(),
        "host assignment must be immutable"
    );
    let mut events = client.subscribe();

    let published = client
        .publish_base(
            TranscriptManifest {
                catalog_revision: "empty".into(),
                total_messages: 0,
                pages: Vec::new(),
                turns: Vec::new(),
            },
            None,
        )
        .await
        .expect("publish base");
    assert_eq!(published.sequence, 1);
    let stale = client
        .publish_delta(
            "missing-page".into(),
            "stale".into(),
            "next".into(),
            TranscriptFrame::Reset { reset: Vec::new() },
        )
        .await
        .expect("stale delta response");
    assert!(stale.need_base);
    assert_eq!(stale.sequence, 1);

    let page = TranscriptPage {
        id: "page-1".into(),
        revision: "base-r".into(),
        first_ordinal: 0,
        messages: Vec::new(),
    };
    let rebased = client
        .publish_base(
            TranscriptManifest {
                catalog_revision: "page-base".into(),
                total_messages: 0,
                pages: vec![TranscriptPageDescriptor {
                    id: "page-1".into(),
                    revision: "base-r".into(),
                    content_hash: None,
                    first_ordinal: 0,
                    message_count: 0,
                    estimated_bytes: 0,
                    previous_page_id: None,
                    next_page_id: None,
                    live: true,
                }],
                turns: Vec::new(),
            },
            Some(page),
        )
        .await
        .expect("publish page base");
    assert_eq!(rebased.sequence, 2);
    let delta = client
        .publish_delta(
            "page-1".into(),
            "base-r".into(),
            "next-r".into(),
            TranscriptFrame::Reset { reset: Vec::new() },
        )
        .await
        .expect("publish matching delta");
    assert!(!delta.need_base);
    assert_eq!(delta.sequence, 3);

    let command_id = format!("command-{}", uuid::Uuid::new_v4().simple());
    let response = reqwest::Client::new()
        .post(format!("{http_base}/hub/{chat}/command"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "id": command_id,
            "kind": "interrupt",
            "payload": { "kind": "interrupt" },
            "issuedBy": "device-b",
            "issuedAt": 100,
            "expiresAt": 9_999_999_999_999_i64
        }))
        .send()
        .await
        .expect("submit command");
    assert!(
        response.status().is_success(),
        "{}",
        response.text().await.unwrap()
    );

    let command = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let SessionHubEvent::Command(command) = events.recv().await.unwrap()
                && command.id == command_id
            {
                return command;
            }
        }
    })
    .await
    .expect("command event");
    assert_eq!(command.status, SessionCommandStatus::Pending);

    let claimed = client.claim_command(&command_id).await.expect("claim");
    let claim_token = claimed.claim_token.expect("claim token");
    let resolved = client
        .resolve_command(
            &command_id,
            &claim_token,
            SessionCommandStatus::Applied,
            Some("done"),
        )
        .await
        .expect("resolve");
    assert_eq!(resolved.status, SessionCommandStatus::Applied);
    assert_eq!(resolved.resolution.as_deref(), Some("done"));

    let command_page: serde_json::Value = reqwest::Client::new()
        .get(format!("{http_base}/hub/{chat}/commands?after=0"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(command_page["commands"][0]["status"], "applied");
    assert!(command_page["nextRevision"].as_u64().unwrap() >= 3);

    let stats: serde_json::Value = reqwest::Client::new()
        .get(format!("{http_base}/hub/{chat}/stats"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(stats["projectionSequence"], 3);
    assert_eq!(stats["commands"]["pending"], 0);

    let sidecar = serde_json::json!({
        "chatId": chat,
        "deviceId": "device-a",
        "checkoutPath": "/tmp",
        "manifest": {
            "catalogRevision": "catalog-1",
            "checkoutId": "checkout-1",
            "deviceId": "device-a",
            "cwd": "/tmp",
            "vcs": "git",
            "files": [],
            "pages": [],
            "additions": 0,
            "deletions": 0,
            "truncated": false,
            "updatedAt": "2026-01-01T00:00:00Z"
        },
        "pages": [],
        "publishedAt": 100
    });
    reqwest::Client::new()
        .post(format!("{http_base}/diff/{chat}"))
        .bearer_auth(token)
        .json(&sidecar)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let wrong_sidecar = serde_json::json!({
        "chatId": chat,
        "deviceId": "device-b",
        "checkoutPath": "/tmp",
        "manifest": {
            "catalogRevision": "catalog-wrong",
            "checkoutId": "checkout-1",
            "deviceId": "device-b",
            "cwd": "/tmp",
            "vcs": "git",
            "files": [],
            "pages": [],
            "additions": 0,
            "deletions": 0,
            "truncated": false,
            "updatedAt": "2026-01-01T00:00:00Z"
        },
        "pages": [],
        "publishedAt": 101
    });
    let wrong_response = reqwest::Client::new()
        .post(format!("{http_base}/diff/{chat}"))
        .bearer_auth(token)
        .json(&wrong_sidecar)
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_response.status(), reqwest::StatusCode::CONFLICT);

    let stored: serde_json::Value = reqwest::Client::new()
        .get(format!("{http_base}/diff/{chat}"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(stored["manifest"]["catalogRevision"], "catalog-1");
}
