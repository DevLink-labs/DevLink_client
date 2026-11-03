use devlink_broker::{
    central::{CentralState, ConnectionStatus},
    protocol::PresenceStatus,
    session_api,
    sessions::{
        Identifier, IdentityError, IdentityFuture, IdentityProvider, ReadyIdentityProvider,
        SessionManager, UnconfiguredIdentityProvider,
    },
};
use reqwest::{Client, Method};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{net::TcpListener, sync::RwLock, task::JoinHandle, time::timeout};
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "synthetic-mcp-session-token-1234";

#[derive(Default)]
struct VerifiedTestIdentity {
    calls: AtomicUsize,
    wrong_agent: bool,
    wrong_bind_user: bool,
    unavailable: bool,
}

impl IdentityProvider for VerifiedTestIdentity {
    fn resolve<'a>(&'a self, agent: Option<&'a Identifier>) -> IdentityFuture<'a> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.unavailable {
                return Err(IdentityError::Unavailable);
            }
            if agent.is_some_and(|id| id.as_str() != "11") {
                return Err(IdentityError::Forbidden);
            }
            let mut value = json!({
                "userId": if agent.is_some() && self.wrong_bind_user { "other-user" } else { "verified-user" },
                "teamId": "verified-team",
            });
            // Only the test provider supplies these values, never the HTTP caller.
            if agent.is_some() {
                value["agentId"] = json!(if self.wrong_agent { "12" } else { "11" });
                value["toolType"] = json!("codex");
            }
            Ok(serde_json::from_value(value).unwrap())
        })
    }
}

struct Fixture {
    url: String,
    client: Client,
    central: CentralState,
    cancel: CancellationToken,
    server: JoinHandle<()>,
    expiration: JoinHandle<()>,
}

impl Fixture {
    async fn new(identity: Arc<dyn IdentityProvider>) -> Self {
        Self::with_provider(|_| identity).await
    }

    async fn with_provider(build: impl FnOnce(CentralState) -> Arc<dyn IdentityProvider>) -> Self {
        let central = CentralState::new(Arc::new(RwLock::new(ConnectionStatus::Connecting)));
        let identity = build(central.clone());
        let manager = SessionManager::new(identity, central.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = session_api::router(Arc::new(TOKEN.into()), manager.clone());
        let cancel = CancellationToken::new();
        let shutdown = cancel.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .unwrap();
        });
        let expiration = tokio::spawn(manager.run_expiration(cancel.clone()));
        Self {
            url,
            client: Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(8))
                .build()
                .unwrap(),
            central,
            cancel,
            server,
            expiration,
        }
    }

    async fn send(&self, method: Method, path: &str, value: Value) -> (u16, Value) {
        let request_id = value["requestId"].clone();
        let response = self
            .client
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(TOKEN)
            .json(&value)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["requestId"], request_id);
        assert_ne!(body.get("data").is_some(), body.get("error").is_some());
        assert!(!body.to_string().contains(TOKEN));
        (status, body)
    }

    async fn claim(&self, instance: &str, mode: &str, agent: Option<&str>) -> (u16, Value) {
        let mut body = json!({ "requestId": format!("claim-{instance}"), "instanceId": instance, "mode": mode });
        if let Some(agent) = agent {
            body["agentId"] = json!(agent);
        }
        self.send(Method::POST, "/v1/sessions", body).await
    }

    async fn stop(mut self) {
        self.cancel.cancel();
        timeout(Duration::from_secs(3), &mut self.server)
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(3), &mut self.expiration)
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn authenticated_ready_identity_flows_through_http_session_bind_and_presence() {
    use devlink_broker::{central, config::Config};
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let f = Fixture::with_provider(|central| Arc::new(ReadyIdentityProvider::new(central))).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = Config {
        server_url: format!("ws://{}", listener.local_addr().unwrap()),
        ..Config::default()
    };
    let runner = tokio::spawn(central::run_with_state(
        config,
        "synthetic-agent-token".into(),
        f.central.clone(),
        f.cancel.clone(),
    ));
    let (stream, _) = listener.accept().await.unwrap();
    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
    socket
        .send(Message::Text(
            json!({"type":"BROKER_READY","payload":{
                "connection_id":"verified-ready","agent_id":12,"user_id":3,"team_id":1
            }})
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    timeout(Duration::from_secs(3), async {
        while f.central.status().await != ConnectionStatus::Connected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let (status, claimed) = f.claim("ready-host", "fresh", None).await;
    assert_eq!(status, 200);
    assert_eq!(
        claimed["data"]["principal"],
        json!({"userId":"3","teamId":"1"})
    );
    let session_id = claimed["data"]["sessionId"].as_str().unwrap();
    let (status, refused) = f
        .send(
            Method::POST,
            &format!("/v1/sessions/{session_id}/bind"),
            json!({"requestId":"wrong-agent","instanceId":"ready-host","agentId":"13"}),
        )
        .await;
    assert_eq!(status, 422);
    assert_eq!(refused["error"]["code"], "FORBIDDEN");
    let (status, bound) = f
        .send(
            Method::POST,
            &format!("/v1/sessions/{session_id}/bind"),
            json!({"requestId":"bind-ready","instanceId":"ready-host","agentId":"12"}),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        bound["data"]["principal"],
        json!({"userId":"3","teamId":"1","agentId":"12"})
    );
    timeout(Duration::from_secs(3), async {
        loop {
            if let Some(Ok(Message::Text(text))) = socket.next().await {
                let value: Value = serde_json::from_str(&text).unwrap();
                if value["type"] == "AGENT_PRESENCE" && value["payload"]["status"] == "ONLINE" {
                    break;
                }
            }
        }
    })
    .await
    .unwrap();
    f.cancel.cancel();
    timeout(Duration::from_secs(3), runner)
        .await
        .unwrap()
        .unwrap();
    f.stop().await;
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.server.abort();
        self.expiration.abort();
    }
}

#[tokio::test]
async fn create_bind_heartbeat_release_preserve_wire_identity_and_presence() {
    let f = Fixture::new(Arc::new(VerifiedTestIdentity::default())).await;
    assert_eq!(f.central.presence().await, None);
    let (status, created) = f.claim("a", "fresh", None).await;
    assert_eq!(status, 200);
    assert_eq!(
        created["data"]["principal"],
        json!({"userId":"verified-user","teamId":"verified-team"})
    );
    let id = created["data"]["sessionId"].as_str().unwrap();
    let repeated = f.claim("a", "recover", None).await;
    assert_eq!(created["data"], repeated.1["data"]);
    assert_eq!(f.central.presence().await, Some(PresenceStatus::Offline));
    let bound = f
        .send(
            Method::POST,
            &format!("/v1/sessions/{id}/bind"),
            json!({"requestId":"bind","instanceId":"a","agentId":"11"}),
        )
        .await;
    assert_eq!(bound.0, 200);
    assert_eq!(bound.1["data"]["sessionId"], id);
    assert_eq!(
        bound.1["data"]["principal"],
        json!({"userId":"verified-user","teamId":"verified-team","agentId":"11","toolType":"codex"})
    );
    assert_eq!(f.central.presence().await, Some(PresenceStatus::Online));
    let beat = f
        .send(
            Method::POST,
            &format!("/v1/sessions/{id}/heartbeat"),
            json!({"requestId":"beat","instanceId":"a"}),
        )
        .await;
    assert_eq!(beat, (200, json!({"requestId":"beat","data":{}})));
    let wrong = f
        .send(
            Method::DELETE,
            &format!("/v1/sessions/{id}"),
            json!({"requestId":"wrong","instanceId":"foreign"}),
        )
        .await;
    assert_eq!(wrong.1["error"]["code"], "SESSION_CONFLICT");
    let removed = f
        .send(
            Method::DELETE,
            &format!("/v1/sessions/{id}"),
            json!({"requestId":"release","instanceId":"a"}),
        )
        .await;
    assert_eq!(removed, (200, json!({"requestId":"release","data":{}})));
    assert_eq!(f.central.presence().await, Some(PresenceStatus::Offline));
    let expired = f
        .send(
            Method::POST,
            &format!("/v1/sessions/{id}/heartbeat"),
            json!({"requestId":"late","instanceId":"a"}),
        )
        .await;
    assert_eq!(expired.1["error"]["code"], "SESSION_EXPIRED");
    f.stop().await;
}

#[tokio::test]
async fn fresh_replaces_and_recover_or_stale_requests_cannot_steal_slot() {
    let f = Fixture::new(Arc::new(VerifiedTestIdentity::default())).await;
    let first = f.claim("a", "fresh", Some("11")).await;
    assert_eq!(first.0, 200);
    let id = first.1["data"]["sessionId"].as_str().unwrap();
    let conflict = f.claim("b", "recover", Some("11")).await;
    assert_eq!(conflict.1["error"]["code"], "SESSION_CONFLICT");
    let second = f.claim("b", "fresh", Some("11")).await;
    assert_eq!(second.0, 200);
    assert_ne!(first.1["data"]["sessionId"], second.1["data"]["sessionId"]);
    for (method, path) in [
        (Method::POST, format!("/v1/sessions/{id}/heartbeat")),
        (Method::DELETE, format!("/v1/sessions/{id}")),
    ] {
        let response = f
            .send(method, &path, json!({"requestId":"stale","instanceId":"a"}))
            .await;
        assert_eq!(response.0, 409);
        assert_eq!(response.1["error"]["code"], "SESSION_REPLACED");
    }
    assert_eq!(
        f.claim("a", "recover", Some("11")).await.1["error"]["code"],
        "SESSION_REPLACED"
    );
    assert_eq!(f.central.presence().await, Some(PresenceStatus::Online));
    assert_eq!(
        f.claim("b", "recover", Some("11")).await.1["data"],
        second.1["data"]
    );
    f.stop().await;
}

#[tokio::test]
async fn auth_origin_and_payload_errors_do_not_create_sessions_or_echo_secrets() {
    let provider = Arc::new(VerifiedTestIdentity::default());
    let f = Fixture::new(provider.clone()).await;
    let valid = json!({"requestId":"validation","instanceId":"a","mode":"fresh"});
    for token in [None, Some("cli-control-token-is-not-mcp-authority")] {
        let mut request = f.client.post(format!("{}/v1/sessions", f.url)).json(&valid);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status().as_u16(), 401);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["scope"],
            "local_auth"
        );
    }
    let origin = f
        .client
        .post(format!("{}/v1/sessions", f.url))
        .bearer_auth(TOKEN)
        .header("origin", "http://localhost")
        .json(&valid)
        .send()
        .await
        .unwrap();
    assert_eq!(origin.status().as_u16(), 403);
    assert_eq!(
        origin.json::<Value>().await.unwrap()["error"]["scope"],
        "local_auth"
    );
    for extra in [
        json!({"userId":"spoofed"}),
        json!({"principal":{"userId":"spoofed"}}),
        json!({"agentId":null}),
        json!({"clientTime":0}),
    ] {
        let mut body = valid.clone();
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let response = f.send(Method::POST, "/v1/sessions", body).await;
        assert_eq!(response.0, 400);
        assert_eq!(response.1["error"]["code"], "VALIDATION_FAILED");
    }
    for raw in [
        "not-json".to_string(),
        r#"{"requestId":"duplicate","instanceId":"a","instanceId":"b","mode":"fresh"}"#.into(),
        format!(
            r#"{{"requestId":"large","instanceId":"a","mode":"fresh","secret":"{}"}}"#,
            TOKEN.repeat(5000)
        ),
    ] {
        let response = f
            .client
            .post(format!("{}/v1/sessions", f.url))
            .bearer_auth(TOKEN)
            .header("content-type", "application/json")
            .body(raw)
            .send()
            .await
            .unwrap();
        assert!([400, 413].contains(&response.status().as_u16()));
        assert!(!response.text().await.unwrap().contains(TOKEN));
    }
    let content_type = f
        .client
        .post(format!("{}/v1/sessions", f.url))
        .bearer_auth(TOKEN)
        .body(valid.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(content_type.status().as_u16(), 415);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.central.presence().await, None);
    f.stop().await;
}

#[tokio::test]
async fn unconfigured_identity_fails_closed_and_transport_failure_remains_unknown() {
    let f = Fixture::new(Arc::new(UnconfiguredIdentityProvider)).await;
    let response = f.claim("a", "fresh", None).await;
    assert_eq!(
        response,
        (
            503,
            json!({"requestId":"claim-a","error":{"code":"CENTRAL_UNAUTHENTICATED","scope":"domain","outcome":"rejected"}})
        )
    );
    assert_eq!(f.central.presence().await, None);
    f.stop().await;
    let f = Fixture::new(Arc::new(VerifiedTestIdentity {
        unavailable: true,
        ..Default::default()
    }))
    .await;
    let response = f.claim("a", "fresh", None).await;
    assert_eq!(response.0, 503);
    assert_eq!(
        response.1["error"],
        json!({"code":"CENTRAL_UNAVAILABLE","scope":"transport","outcome":"unknown"})
    );
    assert_eq!(f.central.presence().await, None);
    f.stop().await;
}

#[tokio::test]
async fn claimed_agent_and_bind_user_must_match_verified_identity() {
    let f = Fixture::new(Arc::new(VerifiedTestIdentity {
        wrong_agent: true,
        ..Default::default()
    }))
    .await;
    assert_eq!(f.claim("a", "fresh", Some("11")).await.0, 422);
    assert_eq!(f.central.presence().await, None);
    f.stop().await;
    let f = Fixture::new(Arc::new(VerifiedTestIdentity {
        wrong_bind_user: true,
        ..Default::default()
    }))
    .await;
    let created = f.claim("a", "fresh", None).await;
    let id = created.1["data"]["sessionId"].as_str().unwrap();
    let response = f
        .send(
            Method::POST,
            &format!("/v1/sessions/{id}/bind"),
            json!({"requestId":"bind","instanceId":"a","agentId":"11"}),
        )
        .await;
    assert_eq!(response.0, 422);
    assert_eq!(f.central.presence().await, Some(PresenceStatus::Offline));
    assert_eq!(
        f.claim("a", "recover", None).await.1["data"],
        created.1["data"]
    );
    f.stop().await;
}

#[tokio::test]
async fn http_session_changes_reach_central_presence_on_the_same_websocket() {
    use devlink_broker::{central, config::Config};
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let f = Fixture::new(Arc::new(VerifiedTestIdentity::default())).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (messages_tx, mut messages_rx) = tokio::sync::mpsc::unbounded_channel();
    let websocket = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket.send(Message::Text(r#"{"type":"BROKER_READY","payload":{"connection_id":"session-test","agent_id":11}}"#.into())).await.unwrap();
        while let Some(Ok(message)) = socket.next().await {
            match message {
                Message::Text(text) => messages_tx
                    .send(serde_json::from_str::<Value>(&text).unwrap())
                    .unwrap(),
                Message::Ping(payload) => socket.send(Message::Pong(payload)).await.unwrap(),
                Message::Close(_) => return,
                _ => {}
            }
        }
    });
    let runner = tokio::spawn(central::run_with_state(
        Config {
            server_url: format!("ws://{address}"),
            ..Config::default()
        },
        "synthetic-central-token".into(),
        f.central.clone(),
        f.cancel.clone(),
    ));
    timeout(Duration::from_secs(3), async {
        while f.central.status().await != ConnectionStatus::Connected {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let created = f.claim("a", "fresh", None).await;
    let id = created.1["data"]["sessionId"].as_str().unwrap();
    assert_eq!(
        timeout(Duration::from_secs(3), messages_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        json!({"type":"AGENT_PRESENCE","payload":{"status":"OFFLINE"}})
    );
    assert_eq!(
        f.send(
            Method::POST,
            &format!("/v1/sessions/{id}/bind"),
            json!({"requestId":"bind","instanceId":"a","agentId":"11"})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        timeout(Duration::from_secs(3), messages_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        json!({"type":"AGENT_PRESENCE","payload":{"status":"ONLINE"}})
    );
    assert_eq!(
        f.send(
            Method::DELETE,
            &format!("/v1/sessions/{id}"),
            json!({"requestId":"release","instanceId":"a"})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        timeout(Duration::from_secs(3), messages_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        json!({"type":"AGENT_PRESENCE","payload":{"status":"OFFLINE"}})
    );
    f.stop().await;
    timeout(Duration::from_secs(3), runner)
        .await
        .unwrap()
        .unwrap();
    timeout(Duration::from_secs(3), websocket)
        .await
        .unwrap()
        .unwrap();
}
