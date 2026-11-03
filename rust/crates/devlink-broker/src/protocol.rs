//! # DevLink Central Protocol
//!
//! 중앙 서버(DevLink Server)의 `/ws/broker` 엔드포인트와 브로커 간 교환하는
//! 확정된 2단계 WebSocket JSON 메시지 규격을 정의하는 모듈입니다.
//!
//! - 연결 인증은 WebSocket Upgrade 요청의 `Authorization: Bearer <Agent 토큰>` 헤더로 수행하며,
//!   연결 수립 후 별도의 `auth` / `hello` 메시지를 송신하지 않습니다.
//! - 서버는 인증·등록 완료 시 `BROKER_READY`를 전송합니다.
//! - 브로커는 유효한 `BROKER_READY` 수신 직후 `AGENT_PRESENCE`로 로컬 Agent 접속 상태(`ONLINE` / `OFFLINE`)를 보고합니다.
//! - 연결 유지는 JSON `HEARTBEAT` PING/PONG과 동일한 `ping_id`로 확인합니다.

use serde::{Deserialize, Serialize};

/// Shared receive limit, also used when validating newly created task payloads.
pub const MAX_CENTRAL_MESSAGE_BYTES: usize = 64 * 1024;

/// 로컬 Agent와 브로커 사이의 접속 상태 (`ONLINE` 또는 `OFFLINE`)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PresenceStatus {
    Online,
    Offline,
}

/// `AGENT_PRESENCE` 메시지 페이로드 규격
///
/// `status` 외의 `agent_id`나 클라이언트 시각(`last_seen_at` 등)을 추가하지 않습니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPresencePayload {
    pub status: PresenceStatus,
}

/// 서버가 만료 감시에 사용하는 JSON PING. WebSocket 제어 Ping과 별개입니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum HeartbeatPing {
    Ping { ping_id: String },
}

/// 서버는 요청의 ping_id를 그대로 돌려줍니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum HeartbeatPong {
    Pong { ping_id: String },
}

/// 브로커에서 중앙 서버로 전송하는 메시지 규격
///
/// # 직렬화(JSON) 예시
/// ```json
/// {
///   "type": "AGENT_PRESENCE",
///   "payload": { "status": "OFFLINE" }
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ClientMessage {
    /// 로컬 Agent 접속 상태 보고 메시지
    AgentPresence(AgentPresencePayload),
    /// 중앙 서버 하트비트 PING 메시지
    Heartbeat(HeartbeatPing),
    /// 중앙 수신 업무(TASK_CREATED) 로컬 영속화 완료 후 전송하는 확인 응답 (ACK)
    Ack(TaskAckPayload),
}

/// 중앙 서버로 전송하는 수신 확인(ACK) 페이로드 (로컬 inbox.json 저장 완료 후 전송)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskAckPayload {
    /// 수신한 업무의 이벤트 고유 식별자
    pub event_id: String,
    /// 수신한 업무의 식별자
    pub task_id: u64,
}

/// 중앙 서버가 브로커로 전달하는 신규 업무(TASK_CREATED) 페이로드
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCreatedPayload {
    /// 업무 식별자
    pub task_id: u64,
    /// 팀 식별자
    pub team_id: u64,
    /// 업무를 요청한 발신 Agent 식별자
    pub requester_agent_id: u64,
    /// 업무를 수신할 대상 Agent 식별자
    pub receiver_agent_id: u64,
    /// 실행할 프롬프트/작업 내용
    pub prompt: String,
    /// 중앙 서버 측 생성 시각 문자열 (LocalDateTime)
    pub created_at: String,
}

/// 중앙 서버가 전달하는 TASK_CREATED 전체 메시지 구조
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCreatedMessage {
    /// 중앙 서버가 발급한 이벤트 식별자 (envelope 레벨)
    pub event_id: String,
    /// 업무 상세 정보 페이로드
    pub payload: TaskCreatedPayload,
}

impl TaskCreatedMessage {
    /// 메시지 필수 필드 및 식별자 범위 유효성 검증
    pub fn is_valid(&self) -> bool {
        !self.event_id.trim().is_empty()
            && self.event_id.encode_utf16().count() <= 128
            && [
                self.payload.task_id,
                self.payload.team_id,
                self.payload.requester_agent_id,
                self.payload.receiver_agent_id,
            ]
            .into_iter()
            .all(|id| id > 0 && id <= i64::MAX as u64)
            && !self.payload.prompt.trim().is_empty()
            && !self.payload.created_at.trim().is_empty()
    }

    /// 수신 확인(ACK)용 페이로드 생성
    pub fn ack(&self) -> TaskAckPayload {
        TaskAckPayload {
            event_id: self.event_id.clone(),
            task_id: self.payload.task_id,
        }
    }
}

impl ClientMessage {
    /// 주어진 접속 상태로 `AGENT_PRESENCE` 메시지를 생성합니다.
    pub fn agent_presence(status: PresenceStatus) -> Self {
        Self::AgentPresence(AgentPresencePayload { status })
    }

    pub fn heartbeat_ping(ping_id: String) -> Self {
        Self::Heartbeat(HeartbeatPing::Ping { ping_id })
    }
}

/// `BROKER_READY` 메시지 페이로드 규격
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerReadyPayload {
    /// 현재 중앙 WebSocket 세션 식별자
    pub connection_id: String,
    /// 인증된 Agent의 숫자 식별자 (양의 정수)
    pub agent_id: u64,
    /// Verified central identity. Both omitted is legacy transport-only READY.
    #[serde(
        default,
        deserialize_with = "crate::sessions::present",
        skip_serializing_if = "Option::is_none"
    )]
    pub user_id: Option<u64>,
    #[serde(
        default,
        deserialize_with = "crate::sessions::present",
        skip_serializing_if = "Option::is_none"
    )]
    pub team_id: Option<u64>,
}

impl BrokerReadyPayload {
    /// `connection_id`가 비어 있지 않고 `agent_id`가 양수인지 검증합니다.
    pub fn is_valid(&self) -> bool {
        let positive = |id| id > 0 && id <= i64::MAX as u64;
        !self.connection_id.trim().is_empty()
            && positive(self.agent_id)
            && match (self.user_id, self.team_id) {
                (None, None) => true,
                (Some(user), Some(team)) => positive(user) && positive(team),
                _ => false,
            }
    }
}

/// 중앙 서버에서 브로커로 수신되는 메시지 규격
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ServerWire", into = "ServerWire")]
pub enum ServerMessage {
    /// 서버 측 인증 및 ConnectionRegistry 등록 완료 응답
    BrokerReady(BrokerReadyPayload),
    /// 하트비트 PONG 응답
    Heartbeat(HeartbeatPong),
    /// 중앙 서버가 전달하는 신규 업무 수신 메시지
    TaskCreated {
        event_id: String,
        payload: TaskCreatedPayload,
    },
}

/// 중앙 서버 WebSocket 메시지 와이어 포맷 (TASK_CREATED의 event_id는 payload 바깥 envelope에 위치)
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
enum ServerWire {
    BrokerReady {
        payload: BrokerReadyPayload,
    },
    Heartbeat {
        payload: HeartbeatPong,
    },
    TaskCreated {
        event_id: String,
        payload: TaskCreatedPayload,
    },
}

impl From<ServerWire> for ServerMessage {
    fn from(value: ServerWire) -> Self {
        match value {
            ServerWire::BrokerReady { payload } => Self::BrokerReady(payload),
            ServerWire::Heartbeat { payload } => Self::Heartbeat(payload),
            ServerWire::TaskCreated { event_id, payload } => {
                Self::TaskCreated { event_id, payload }
            }
        }
    }
}

impl From<ServerMessage> for ServerWire {
    fn from(value: ServerMessage) -> Self {
        match value {
            ServerMessage::BrokerReady(payload) => Self::BrokerReady { payload },
            ServerMessage::Heartbeat(payload) => Self::Heartbeat { payload },
            ServerMessage::TaskCreated { event_id, payload } => {
                Self::TaskCreated { event_id, payload }
            }
        }
    }
}

impl ServerMessage {
    pub fn heartbeat_pong(ping_id: String) -> Self {
        Self::Heartbeat(HeartbeatPong::Pong { ping_id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_identity_fields_are_positive_numeric_pairs() {
        let base = serde_json::json!({"type":"BROKER_READY","payload":{
            "connection_id":"verified","agent_id":12,"user_id":3,"team_id":1
        }});
        let message: ServerMessage = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(serde_json::to_value(&message).unwrap(), base);
        let ServerMessage::BrokerReady(ready) = message else {
            panic!("READY required")
        };
        assert!(ready.is_valid());
        assert_eq!((ready.user_id, ready.team_id), (Some(3), Some(1)));
        for (field, value) in [
            ("user_id", serde_json::json!(null)),
            ("team_id", serde_json::json!("1")),
            ("user_id", serde_json::json!(-1)),
            ("team_id", serde_json::json!(0)),
            ("user_id", serde_json::json!(u64::MAX)),
        ] {
            let mut invalid = base.clone();
            invalid["payload"][field] = value;
            let result = serde_json::from_value::<ServerMessage>(invalid);
            assert!(!matches!(result, Ok(ServerMessage::BrokerReady(ready)) if ready.is_valid()));
        }
        let mut partial = base;
        partial["payload"]
            .as_object_mut()
            .unwrap()
            .remove("team_id");
        let ServerMessage::BrokerReady(ready) = serde_json::from_value(partial).unwrap() else {
            panic!("READY required")
        };
        assert!(!ready.is_valid());
    }

    #[test]
    fn broker_ready_parses_valid_payload_and_rejects_invalid_shapes() {
        let raw = r#"{
            "type": "BROKER_READY",
            "payload": {
                "connection_id": "current-websocket-session-id",
                "agent_id": 11
            }
        }"#;
        let msg: ServerMessage = serde_json::from_str(raw).expect("valid BROKER_READY must parse");
        let ServerMessage::BrokerReady(payload) = msg else {
            panic!("expected BROKER_READY");
        };
        assert!(payload.is_valid());
        assert_eq!(payload.connection_id, "current-websocket-session-id");
        assert_eq!(payload.agent_id, 11);

        // Empty connection_id or zero agent_id must be rejected by validation
        let empty_conn: ServerMessage = serde_json::from_str(
            r#"{"type":"BROKER_READY","payload":{"connection_id":"   ","agent_id":11}}"#,
        )
        .unwrap();
        let ServerMessage::BrokerReady(empty_payload) = empty_conn else {
            panic!("expected BROKER_READY");
        };
        assert!(!empty_payload.is_valid());

        let zero_agent: ServerMessage = serde_json::from_str(
            r#"{"type":"BROKER_READY","payload":{"connection_id":"conn-1","agent_id":0}}"#,
        )
        .unwrap();
        let ServerMessage::BrokerReady(zero_payload) = zero_agent else {
            panic!("expected BROKER_READY");
        };
        assert!(!zero_payload.is_valid());

        // Malformed types, string agent_id, unknown fields, and legacy messages must fail deserialization
        for invalid in [
            r#"{"type":"auth_ok"}"#,
            r#"{"type":"BROKER_READY","payload":{"connection_id":"c1","agent_id":"11"}}"#,
            r#"{"type":"BROKER_READY","payload":{"connection_id":"c1","agent_id":-1}}"#,
            r#"{"type":"BROKER_READY","payload":{"connection_id":"c1","agent_id":11,"extra":true}}"#,
            r#"{"type":"BROKER_READY","payload":{"connection_id":"c1"}}"#,
            r#"{"type":"BROKER_READY"}"#,
        ] {
            assert!(
                serde_json::from_str::<ServerMessage>(invalid).is_err(),
                "expected invalid ServerMessage to be rejected"
            );
        }
    }

    #[test]
    fn agent_presence_serializes_exact_contract_without_extra_fields() {
        let online = serde_json::to_value(ClientMessage::agent_presence(PresenceStatus::Online))
            .expect("online presence must serialize");
        assert_eq!(
            online,
            serde_json::json!({
                "type": "AGENT_PRESENCE",
                "payload": { "status": "ONLINE" }
            })
        );

        let offline = serde_json::to_value(ClientMessage::agent_presence(PresenceStatus::Offline))
            .expect("offline presence must serialize");
        assert_eq!(
            offline,
            serde_json::json!({
                "type": "AGENT_PRESENCE",
                "payload": { "status": "OFFLINE" }
            })
        );
    }

    #[test]
    fn heartbeat_uses_exact_json_ping_pong_contract() {
        assert_eq!(
            serde_json::to_value(ClientMessage::heartbeat_ping("ping-1".into())).unwrap(),
            serde_json::json!({"type":"HEARTBEAT","payload":{"kind":"PING","ping_id":"ping-1"}})
        );
        let pong = r#"{"type":"HEARTBEAT","payload":{"kind":"PONG","ping_id":"ping-1"}}"#;
        assert_eq!(
            serde_json::from_str::<ServerMessage>(pong).unwrap(),
            ServerMessage::heartbeat_pong("ping-1".into())
        );
        for payload in [
            r#"{"kind":"PING","ping_id":"ping-1"}"#,
            r#"{"kind":"PONG"}"#,
            r#"{"kind":"PONG","ping_id":1}"#,
            r#"{"kind":"PONG","ping_id":"ping-1","agent_id":11}"#,
            r#"{"kind":"UNKNOWN","ping_id":"ping-1"}"#,
        ] {
            let raw = format!(r#"{{"type":"HEARTBEAT","payload":{payload}}}"#);
            assert!(serde_json::from_str::<ServerMessage>(&raw).is_err());
        }
        assert!(serde_json::from_str::<ServerMessage>(&format!("{pong} {{}}")).is_err());
    }
}
