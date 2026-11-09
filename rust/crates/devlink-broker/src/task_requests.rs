//! 작업 생성 요청의 영속 키 관리자 (중앙 멱등 정책을 대체하지 않음)
//!
//! HTTP 요청을 보내기 전 서버 멱등키(UUID)를 파일에 안전하게 영속화합니다.
//! 동일한 클라이언트가 동일한 requestId로 재요청할 경우 기발급된 동일 멱등키를 재사용하여 중복 작업을 방지합니다.

use crate::{
    config, credentials, private_file,
    sessions::{Identifier, Principal},
    task_client::{CreateTaskCommand, TaskError, numeric_id},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// 최대 보관 요청 수 (1,024건)
const MAX_RECORDS: usize = 1024;
/// 로그 파일 최대 크기 (64 MB)
const MAX_BYTES: usize = 64 * 1024 * 1024;

/// 영속 저장되는 개별 작업 생성 요청 레코드
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    origin: String,
    user_id: u64,
    team_id: u64,
    request_id: String,
    command: CreateTaskCommand,
}

/// 파일에 저장되는 작업 요청 스냅샷 DTO
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    schema_version: u32,
    records: Vec<Record>,
}

/// 인메모리 요청 캐시 및 파일 저장소 제어 구조체
struct Store {
    path: PathBuf,
    records: Vec<Record>,
    gates: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
    failed: bool,
}

/// 작업 요청 멱등성 관리자 (공유 핸들)
#[derive(Clone)]
pub struct TaskRequests {
    store: Arc<Mutex<Store>>,
    writer: Arc<tokio::sync::Semaphore>,
}

/// 멱등키가 발급/확인된 준비 완료 작업 및 동시성 게이트
pub struct PreparedTask {
    pub command: CreateTaskCommand,
    pub gate: Arc<tokio::sync::Mutex<()>>,
}

/// Preparation failed before central dispatch. A published key is returned
/// under the same store lock as conflict detection, only to its original actor.
#[derive(Debug)]
pub struct PrepareError {
    pub error: TaskError,
    pub request_key: Option<String>,
}

impl From<TaskError> for PrepareError {
    fn from(error: TaskError) -> Self {
        Self {
            error,
            request_key: None,
        }
    }
}

impl TaskRequests {
    /// 상태 디렉토리 내의 task-requests.json 경로 반환
    pub fn path(config_path: &Path) -> PathBuf {
        config::state_directory(config_path).join("task-requests.json")
    }

    /// 파일로부터 기존 요청 로그를 검증 및 로드하여 TaskRequests 인스턴스 초기화
    pub fn open(config_path: &Path) -> Result<Self> {
        let path = Self::path(config_path);
        let parent = path.parent().context("Task request log has no parent")?;
        credentials::ensure_safe_parent_directory(parent)?;
        let exists = credentials::ensure_safe_target_if_exists(&path)?;
        if exists {
            credentials::verify_private_permissions(&path)?;
        }
        credentials::set_private_permissions(parent)?;
        let records = if exists {
            let mut bytes = Vec::new();
            File::open(&path)?
                .take(MAX_BYTES as u64 + 1)
                .read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= MAX_BYTES, "Task request log is too large");
            let snapshot: Snapshot = serde_json::from_slice(&bytes)
                .map_err(|_| anyhow::anyhow!("Invalid task request log"))?;
            ensure!(
                snapshot.schema_version == 1 && snapshot.records.len() <= MAX_RECORDS,
                "Unsupported task request log"
            );
            let mut requests = HashSet::new();
            let mut keys = HashSet::new();
            for record in &snapshot.records {
                ensure!(
                    valid_origin(&record.origin)
                        && numeric_id(&record.user_id.to_string()).is_some()
                        && numeric_id(&record.team_id.to_string()).is_some()
                        && record.command.valid()
                        && Identifier::parse(record.request_id.clone()).is_ok(),
                    "Invalid task request record"
                );
                ensure!(
                    requests.insert((
                        record.origin.as_str(),
                        record.user_id,
                        record.request_id.as_str()
                    )) && keys.insert(record.command.request_key.as_str()),
                    "Duplicate task request record"
                );
            }
            snapshot.records
        } else {
            Vec::new()
        };
        Ok(Self {
            store: Arc::new(Mutex::new(Store {
                path,
                records,
                gates: HashMap::new(),
                failed: false,
            })),
            writer: Arc::new(tokio::sync::Semaphore::new(1)),
        })
    }

    /// 작업 요청을 검증하고, 멱등키를 할당하거나 기존 키를 재사용하여 준비 상태를 생성
    ///
    /// `command.request_key`가 비어 있으면 requestId 기준으로 키를 발급·재사용한다.
    /// 호출자가 키를 주면 그 키가 논리 요청의 식별자다(S15P31D108-165): 같은 행위자의
    /// 같은 내용이면 기존 기록을 재사용하고, 다른 내용·다른 행위자면 충돌이다.
    pub async fn prepare(
        &self,
        origin: String,
        principal: Principal,
        request_id: Identifier,
        mut command: CreateTaskCommand,
    ) -> Result<PreparedTask, PrepareError> {
        let user_id = numeric_id(principal.user_id.as_str()).ok_or_else(validation)?;
        let team_id = numeric_id(principal.team_id.as_str()).ok_or_else(validation)?;
        let agent = principal
            .agent_id
            .as_ref()
            .and_then(|id| numeric_id(id.as_str()))
            .ok_or_else(validation)?;
        if agent != command.requester_agent_id || !valid_origin(&origin) {
            return Err(validation().into());
        }
        let caller_key = !command.request_key.is_empty();
        if caller_key && Identifier::parse(command.request_key.clone()).is_err() {
            return Err(validation().into());
        }
        // Validate before IO without minting another key for an existing request.
        if !caller_key {
            command.request_key = "pending".into();
        }
        if !command.valid() {
            return Err(validation().into());
        }
        let record = Record {
            origin,
            user_id,
            team_id,
            request_id: request_id.as_str().into(),
            command,
        };
        // 단일 파일 쓰기 세마포어 획득 후 블로킹 스레드풀에서 동기 디스크 I/O 실행
        let permit = self
            .writer
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| storage_error())?;
        let log = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            log.prepare_sync(record, caller_key)
        })
        .await
        .map_err(|_| storage_error())?
    }

    /// 동기 방식으로 요청 로그 검사, 중복/재시도 판정 및 파일 영속화 수행
    fn prepare_sync(
        &self,
        mut incoming: Record,
        caller_key: bool,
    ) -> Result<PreparedTask, PrepareError> {
        let mut store = self.store.lock().map_err(|_| storage_error())?;
        let by_request = store.records.iter().position(|record| {
            record.origin == incoming.origin
                && record.user_id == incoming.user_id
                && record.request_id == incoming.request_id
        });
        let by_key = caller_key
            .then(|| {
                store
                    .records
                    .iter()
                    .position(|record| record.command.request_key == incoming.command.request_key)
            })
            .flatten();
        let known_key = if caller_key {
            // The caller supplied this key, so echo it unless another actor owns it.
            by_key
                .is_none_or(|index| same_actor(&store.records[index], &incoming))
                .then(|| incoming.command.request_key.clone())
        } else {
            by_request
                .filter(|index| same_actor(&store.records[*index], &incoming))
                .map(|index| store.records[index].command.request_key.clone())
        };
        let storage_failure = || PrepareError {
            error: storage_error(),
            request_key: known_key.clone(),
        };
        let conflict = || PrepareError {
            error: TaskError::rejected("REQUEST_KEY_CONFLICT", 409),
            request_key: known_key.clone(),
        };
        if store.failed {
            return Err(storage_failure());
        }
        let parent = store.path.parent().ok_or_else(storage_failure)?;
        credentials::verify_parent_safety(parent).map_err(|_| storage_failure())?;
        let exists = credentials::ensure_safe_target_if_exists(&store.path)
            .map_err(|_| storage_failure())?;
        if !exists && !store.records.is_empty() {
            return Err(storage_failure());
        }
        // 기존 기록 선택: 호출자 키는 키로, 그 외에는 (origin, user_id, request_id)로 찾는다.
        let previous = if caller_key {
            // A requestId already bound to another logical request cannot be renamed.
            if by_request.is_some_and(|index| Some(index) != by_key) {
                return Err(conflict());
            }
            by_key
        } else {
            by_request
        };
        if let Some(index) = previous {
            let previous = &store.records[index];
            // 동일 요청이 존재하면 기발급된 request_key를 그대로 재사용
            incoming.command.request_key = previous.command.request_key.clone();
            // 단, 행위자·팀·요청 내용이 다르면 충돌(409) 에러 반환
            if !same_actor(previous, &incoming) || previous.command != incoming.command {
                return Err(conflict());
            }
        } else {
            // 신규 요청인 경우 최대 보관 건수 검사 후 파일에 스냅샷 저장
            if store.records.len() >= MAX_RECORDS {
                return Err(storage_failure());
            }
            if !caller_key {
                incoming.command.request_key = uuid::Uuid::new_v4().to_string();
            }
            let mut records = store.records.clone();
            records.push(incoming.clone());
            if private_file::write(
                &store.path,
                &Snapshot {
                    schema_version: 1,
                    records: records.clone(),
                },
                MAX_BYTES,
            )
            .is_err()
            {
                store.failed = true;
                return Err(storage_failure());
            }
            store.records = records;
        }
        // 동일 request_key를 공유하는 동시성 제어 게이트 락(Mutex) 반환
        let gate = store
            .gates
            .entry(incoming.command.request_key.clone())
            .or_default()
            .clone();
        Ok(PreparedTask {
            command: incoming.command,
            gate,
        })
    }
}

/// The same actor in the same team may learn a key; another actor never does.
fn same_actor(record: &Record, incoming: &Record) -> bool {
    record.origin == incoming.origin
        && record.user_id == incoming.user_id
        && record.team_id == incoming.team_id
        && record.command.requester_agent_id == incoming.command.requester_agent_id
}

fn valid_origin(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.has_host()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == "/"
            && url.as_str() == value
    })
}

fn validation() -> TaskError {
    TaskError::rejected("VALIDATION_FAILED", 400)
}
fn storage_error() -> TaskError {
    TaskError::rejected("BROKER_STORAGE_FAILED", 503)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn actor() -> Principal {
        Principal {
            user_id: Identifier::parse("3").unwrap(),
            team_id: Identifier::parse("1").unwrap(),
            agent_id: Some(Identifier::parse("12").unwrap()),
            tool_type: None,
        }
    }
    fn command() -> CreateTaskCommand {
        CreateTaskCommand {
            request_key: String::new(),
            requester_agent_id: 12,
            receiver_agent_id: 13,
            prompt: "synthetic prompt".into(),
        }
    }

    #[tokio::test]
    async fn request_log_reuses_keys_isolates_origins_and_rejects_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let log = TaskRequests::open(&path).unwrap();
        let id = Identifier::parse("request-1").unwrap();
        let first = log
            .prepare(
                "http://localhost:8080/".into(),
                actor(),
                id.clone(),
                command(),
            )
            .await
            .unwrap();
        drop(log);
        let log = TaskRequests::open(&path).unwrap();
        let replay = log
            .prepare(
                "http://localhost:8080/".into(),
                actor(),
                id.clone(),
                command(),
            )
            .await
            .unwrap();
        assert_eq!(first.command.request_key, replay.command.request_key);
        let other = log
            .prepare(
                "http://localhost:8081/".into(),
                actor(),
                id.clone(),
                command(),
            )
            .await
            .unwrap();
        assert_ne!(first.command.request_key, other.command.request_key);
        let mut changed = command();
        changed.prompt = "changed".into();
        let conflict = log
            .prepare(
                "http://localhost:8080/".into(),
                actor(),
                id.clone(),
                changed,
            )
            .await
            .err()
            .unwrap();
        assert_eq!(conflict.error.code, "REQUEST_KEY_CONFLICT");
        assert_eq!(
            conflict.request_key.as_deref(),
            Some(first.command.request_key.as_str())
        );
        let mut other_team = actor();
        other_team.team_id = Identifier::parse("99").unwrap();
        let conflict = log
            .prepare(
                "http://localhost:8080/".into(),
                other_team,
                id.clone(),
                command(),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(conflict.error.code, "REQUEST_KEY_CONFLICT");
        assert!(
            conflict.request_key.is_none(),
            "another team must not see the existing key"
        );
        let mut other_agent = actor();
        other_agent.agent_id = Some(Identifier::parse("99").unwrap());
        let mut other_command = command();
        other_command.requester_agent_id = 99;
        let conflict = log
            .prepare(
                "http://localhost:8080/".into(),
                other_agent,
                id.clone(),
                other_command,
            )
            .await
            .err()
            .unwrap();
        assert_eq!(conflict.error.code, "REQUEST_KEY_CONFLICT");
        assert!(
            conflict.request_key.is_none(),
            "another Agent must not see the existing key"
        );
        // A later disk failure must not erase a key already published to this
        // actor. Lookup and failure metadata share the preparation store lock.
        let saved = std::fs::read(TaskRequests::path(&path)).unwrap();
        std::fs::remove_file(TaskRequests::path(&path)).unwrap();
        std::fs::create_dir(TaskRequests::path(&path)).unwrap();
        let failure = log
            .prepare("http://localhost:8080/".into(), actor(), id, command())
            .await
            .err()
            .unwrap();
        assert_eq!(failure.error.code, "BROKER_STORAGE_FAILED");
        assert_eq!(
            failure.request_key.as_deref(),
            Some(first.command.request_key.as_str())
        );
        std::fs::remove_dir(TaskRequests::path(&path)).unwrap();
        std::fs::write(TaskRequests::path(&path), saved).unwrap();
        // Unix umask can leave a recreated file group/other-readable; restore the
        // private mode so the corruption check below is not a permission failure.
        credentials::set_private_permissions(&TaskRequests::path(&path)).unwrap();
        credentials::verify_private_permissions(&TaskRequests::path(&path)).unwrap();
        std::fs::write(TaskRequests::path(&path), b"partial-json").unwrap();
        assert!(TaskRequests::open(&path).is_err());
        assert_eq!(
            std::fs::read(TaskRequests::path(&path)).unwrap(),
            b"partial-json"
        );
    }

    #[tokio::test]
    async fn caller_keys_identify_requests_and_never_leak_to_other_actors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let origin = || "http://localhost:8080/".to_string();
        let keyed = |key: &str| CreateTaskCommand {
            request_key: key.into(),
            ..command()
        };
        let id = |value: &str| Identifier::parse(value).unwrap();
        let log = TaskRequests::open(&path).unwrap();
        let first = log
            .prepare(origin(), actor(), id("call-1"), keyed("agent-key"))
            .await
            .unwrap();
        assert_eq!(first.command.request_key, "agent-key");
        // A new requestId with the same key and content is the same logical request.
        let retry = log
            .prepare(origin(), actor(), id("call-2"), keyed("agent-key"))
            .await
            .unwrap();
        assert_eq!(retry.command, first.command);
        assert!(Arc::ptr_eq(&retry.gate, &first.gate));
        let mut changed = keyed("agent-key");
        changed.prompt = "changed".into();
        let conflict = log
            .prepare(origin(), actor(), id("call-3"), changed)
            .await
            .err()
            .unwrap();
        assert_eq!(conflict.error.code, "REQUEST_KEY_CONFLICT");
        assert_eq!(conflict.request_key.as_deref(), Some("agent-key"));
        // A requestId already bound to one key cannot be reused under another key.
        let renamed = log
            .prepare(origin(), actor(), id("call-1"), keyed("other-key"))
            .await
            .err()
            .unwrap();
        assert_eq!(renamed.error.code, "REQUEST_KEY_CONFLICT");
        let mut other_team = actor();
        other_team.team_id = Identifier::parse("99").unwrap();
        let mut other_agent = actor();
        other_agent.agent_id = Some(Identifier::parse("99").unwrap());
        let other_command = CreateTaskCommand {
            requester_agent_id: 99,
            ..keyed("agent-key")
        };
        for (principal, command) in [
            (other_team, keyed("agent-key")),
            (other_agent, other_command),
        ] {
            let conflict = log
                .prepare(origin(), principal, id("foreign"), command)
                .await
                .err()
                .unwrap();
            assert_eq!(conflict.error.code, "REQUEST_KEY_CONFLICT");
            assert!(conflict.request_key.is_none(), "another actor's key leaked");
        }
        let long = "x".repeat(65);
        for invalid in ["a b", " key", long.as_str(), "\u{3000}"] {
            let error = log
                .prepare(origin(), actor(), id("invalid"), keyed(invalid))
                .await
                .err()
                .unwrap();
            assert_eq!(error.error.code, "VALIDATION_FAILED", "{invalid:?}");
        }
        // A broker-issued key can be passed back as the caller key.
        let minted = log
            .prepare(origin(), actor(), id("plain"), command())
            .await
            .unwrap();
        let reused = log
            .prepare(
                origin(),
                actor(),
                id("plain-retry"),
                keyed(&minted.command.request_key),
            )
            .await
            .unwrap();
        assert_eq!(reused.command, minted.command);
        drop(log);
        let log = TaskRequests::open(&path).unwrap();
        let restarted = log
            .prepare(origin(), actor(), id("call-4"), keyed("agent-key"))
            .await
            .unwrap();
        assert_eq!(restarted.command, first.command);
        let snapshot: serde_json::Value =
            serde_json::from_slice(&std::fs::read(TaskRequests::path(&path)).unwrap()).unwrap();
        assert_eq!(snapshot["records"].as_array().unwrap().len(), 2);
    }
}
