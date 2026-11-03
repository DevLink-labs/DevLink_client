//! Stage 2 central server mock (`/ws/broker`). Never prints authentication payloads.
#![allow(clippy::result_large_err)]
use anyhow::{Context, Result};
use devlink_broker::protocol::{
    BrokerReadyPayload, ClientMessage, HeartbeatPing, ServerMessage, TaskCreatedMessage,
    TaskCreatedPayload,
};
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::{
    net::TcpListener,
    time::{Duration, Instant, sleep},
};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{ErrorResponse, Request, Response},
        http::{StatusCode, header::AUTHORIZATION},
        protocol::CloseFrame,
    },
};

static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

#[tokio::main]
async fn main() -> Result<()> {
    let token = std::env::var("DEVLINK_TOKEN").context("Set DEVLINK_TOKEN for the mock server")?;
    // Synthetic demo only: repeated deliveries use new event IDs for the same task.
    let demo_task = std::env::var("DEVLINK_MOCK_TASK_DEMO").is_ok_and(|value| value == "1");
    let addr = std::env::var("DEVLINK_MOCK_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let listener = TcpListener::bind(&addr).await?;
    println!(
        "Mock central server listening on {}",
        listener.local_addr()?
    );
    loop {
        let (stream, _) = tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            accepted = listener.accept() => accepted?,
        };
        let expected_bearer = format!("Bearer {token}");
        tokio::spawn(async move {
            let handshake = accept_hdr_async(
                stream,
                move |req: &Request,
                      response: Response|
                      -> std::result::Result<Response, ErrorResponse> {
                    if req.uri().path() != "/ws/broker" || req.uri().query().is_some() {
                        let mut err = ErrorResponse::new(None);
                        *err.status_mut() = StatusCode::UNAUTHORIZED;
                        return Err(err);
                    }
                    let authorized = req
                        .headers()
                        .get(AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|v| v == expected_bearer);
                    if !authorized {
                        let mut err = ErrorResponse::new(None);
                        *err.status_mut() = StatusCode::UNAUTHORIZED;
                        return Err(err);
                    }
                    Ok(response)
                },
            );
            let Ok(Ok(mut socket)) =
                tokio::time::timeout(std::time::Duration::from_secs(5), handshake).await
            else {
                return;
            };

            let seq = NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed);
            let ready = serde_json::to_string(&ServerMessage::BrokerReady(BrokerReadyPayload {
                connection_id: format!("mock-session-{seq}"),
                agent_id: 1,
                user_id: Some(1),
                team_id: Some(1),
            }))
            .expect("serializable BROKER_READY");

            if socket.send(Message::Text(ready.into())).await.is_err() {
                return;
            }

            let mut pending = std::collections::HashMap::new();
            if demo_task {
                for attempt in 1..=2 {
                    let event_id = format!("mock-{seq}-{attempt}");
                    let payload = TaskCreatedPayload {
                        task_id: 101,
                        team_id: 1,
                        requester_agent_id: 2,
                        receiver_agent_id: 1,
                        prompt: "Synthetic inbox demo; do not execute this task.".into(),
                        created_at: "2026-10-29T09:00:00".into(),
                    };
                    let task = TaskCreatedMessage {
                        event_id: event_id.clone(),
                        payload: payload.clone(),
                    };
                    pending.insert(event_id.clone(), task.payload.task_id);
                    let message = ServerMessage::TaskCreated { event_id, payload };
                    if socket
                        .send(Message::Text(
                            serde_json::to_string(&message).unwrap().into(),
                        ))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }

            // 운영 서버처럼 JSON PING만 만료 감시 시간을 갱신합니다.
            let deadline = sleep(Duration::from_secs(40));
            tokio::pin!(deadline);
            loop {
                let message = tokio::select! {
                    _ = &mut deadline => {
                        let _ = socket.close(Some(CloseFrame {
                            code: 4001.into(),
                            reason: "Heartbeat timeout".into(),
                        })).await;
                        break;
                    }
                    message = socket.next() => message,
                };
                let Some(Ok(message)) = message else { break };
                match message {
                    Message::Text(text) => match serde_json::from_str::<ClientMessage>(&text) {
                        Ok(ClientMessage::AgentPresence(_)) => {}
                        Ok(ClientMessage::Ack(ack)) => {
                            if pending.get(&ack.event_id) == Some(&ack.task_id) {
                                pending.remove(&ack.event_id);
                                println!(
                                    "Synthetic task receipt acknowledged ({}/2)",
                                    2 - pending.len()
                                );
                            }
                        }
                        Ok(ClientMessage::Heartbeat(HeartbeatPing::Ping { ping_id }))
                            if !ping_id.trim().is_empty()
                                && ping_id.encode_utf16().count() <= 128 =>
                        {
                            deadline
                                .as_mut()
                                .reset(Instant::now() + Duration::from_secs(40));
                            let pong =
                                serde_json::to_string(&ServerMessage::heartbeat_pong(ping_id))
                                    .expect("serializable HEARTBEAT PONG");
                            if socket.send(Message::Text(pong.into())).await.is_err() {
                                break;
                            }
                        }
                        _ => {
                            let _ = socket
                                .close(Some(CloseFrame {
                                    code: 1008.into(),
                                    reason: "Invalid message".into(),
                                }))
                                .await;
                            break;
                        }
                    },
                    Message::Ping(payload) => {
                        if socket.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        });
    }
}
