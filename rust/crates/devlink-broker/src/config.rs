//! # DevLink Configuration Management
//!
//! DevLink의 설정 파일(`config.json`) 로드, 저장, 원자적 쓰기(atomic write), 파일 락(lock),
//! 유효성 검사 및 동적 수정을 담당하는 모듈입니다.

use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

/// DevLink 브로커 설정 구조체
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    /// 중앙 웹소켓 서버 주소 (wss://, ws://는 루프백만)
    pub server_url: String,
    /// 에이전트 식별자 (예: codex, claude 등)
    pub agent: String,
    /// 연결 단절 시 자동 재연결 여부
    pub auto_reconnect: bool,
    /// 로컬 제어 HTTP 서버 바인딩 포트 (기본값: 43127)
    #[serde(default = "default_port")]
    pub local_port: u16,
    /// A new setup profile must save the first port it successfully binds.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub local_port_pending: bool,
    /// 재연결 초기 대기 시간 (ms, 기본값: 2000)
    #[serde(default = "initial_delay")]
    pub reconnect_initial_ms: u64,
    /// 재연결 최대 대기 시간 (ms, 기본값: 30000)
    #[serde(default = "max_delay")]
    pub reconnect_max_ms: u64,
    /// 중앙 서버 인증 응답 제한 시간 (ms, 기본값: 5000)
    #[serde(default = "auth_timeout")]
    pub auth_timeout_ms: u64,
    /// 질문·답변 안전망 pull 주기 (ms, 기본값: 60000, 실제 대기는 ±1/6 무작위)
    #[serde(default = "task_message_sync_interval")]
    pub task_message_sync_interval_ms: u64,
    /// 보낸 작업에 이 시간(분) 동안 완료·실패·거절 알림이 없으면 로컬 지연 알림을 남김 (기본값: 30, 0이면 끔)
    #[serde(default = "task_overdue_minutes")]
    pub task_overdue_minutes: u64,
    /// Git remote for the repository key; empty means `origin`. Also turns on the dependency
    /// graph upload, which is sent only when this is set.
    #[serde(default)]
    pub dependency_remote: String,
    /// Upload the bound project's working files (IDX-3-02) and commit history (IDX-3-01).
    /// Default on; `false` stops both collectors after a broker restart.
    #[serde(default = "git_collection")]
    pub git_collection: bool,
}

pub(crate) const DEFAULT_LOCAL_PORT: u16 = 43127;

fn default_port() -> u16 {
    DEFAULT_LOCAL_PORT
}
fn initial_delay() -> u64 {
    2000
}
fn max_delay() -> u64 {
    30000
}
fn auth_timeout() -> u64 {
    5000
}
fn task_message_sync_interval() -> u64 {
    60000
}
fn task_overdue_minutes() -> u64 {
    30
}
fn git_collection() -> bool {
    true
}

/// Safe field diagnostics, shared by config commands and setup.
#[derive(Debug)]
pub struct ConfigValidationError {
    pub field: &'static str,
    message: &'static str,
}

impl std::fmt::Display for ConfigValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message)
    }
}
impl std::error::Error for ConfigValidationError {}

pub(crate) const REMOTE_REQUIRES_TLS: &str =
    "serverUrl must use wss:// (HTTPS) for remote servers; ws:// is allowed only on loopback";

/// 평문(ws/http) 연결을 허용해도 되는 이 PC 안(루프백) 호스트인지 확인합니다.
/// 외부 서버로는 Agent 토큰과 사용자 세션이 평문으로 나가지 않도록 wss/https만 씁니다.
pub(crate) fn is_loopback_host(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

pub(crate) fn invalid(field: &'static str, message: &'static str) -> anyhow::Error {
    ConfigValidationError { field, message }.into()
}

impl Default for Config {
    /// 기본 권장 설정값 반환
    fn default() -> Self {
        Self {
            server_url: "wss://api.devlink.ai.kr".into(),
            agent: "codex".into(),
            auto_reconnect: true,
            local_port: default_port(),
            local_port_pending: false,
            reconnect_initial_ms: initial_delay(),
            reconnect_max_ms: max_delay(),
            auth_timeout_ms: auth_timeout(),
            task_message_sync_interval_ms: task_message_sync_interval(),
            task_overdue_minutes: task_overdue_minutes(),
            dependency_remote: String::new(),
            git_collection: git_collection(),
        }
    }
}

impl Config {
    /// 설정값들의 유효성을 엄격하게 검증합니다.
    /// - `server_url`: wss 스킴 필수(ws는 루프백 호스트만), 경로는 루트(`""` 또는 `"/"`) 또는 `"/ws/broker"`만 허용, 쿼리/프래그먼트/사용자정보 금지
    /// - `agent`: 1~64자의 영숫자 및 `-`, `_` 허용
    /// - `reconnect_*`: 범위 및 max >= initial 조건 확인
    /// - `auth_timeout_ms`: 100ms ~ 60000ms 범위 확인
    pub fn validate(&self) -> Result<()> {
        if self.dependency_remote.starts_with('-')
            || self.dependency_remote.len() > 64
            || !self
                .dependency_remote
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        {
            return Err(invalid(
                "dependencyRemote",
                "dependencyRemote must be a Git remote name or empty",
            ));
        }
        let url = url::Url::parse(&self.server_url)
            .map_err(|_| invalid("serverUrl", "serverUrl must be a valid ws:// or wss:// URL"))?;
        if !matches!(url.scheme(), "ws" | "wss")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid(
                "serverUrl",
                "serverUrl requires ws/wss and a host; credentials, query and fragment are forbidden",
            ));
        }
        if url.scheme() == "ws" && !is_loopback_host(&url) {
            return Err(invalid("serverUrl", REMOTE_REQUIRES_TLS));
        }
        if !matches!(url.path(), "" | "/" | "/ws/broker") {
            return Err(invalid(
                "serverUrl",
                "serverUrl path must be empty, '/', or '/ws/broker'",
            ));
        }
        if self.agent.is_empty()
            || self.agent.len() > 64
            || !self
                .agent
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
        {
            return Err(invalid(
                "agent",
                "agent must contain 1-64 letters, digits, hyphens or underscores",
            ));
        }
        if self.reconnect_initial_ms < 50
            || self.reconnect_max_ms < self.reconnect_initial_ms
            || self.reconnect_max_ms > 300000
        {
            return Err(invalid(
                "reconnectInitialMs/reconnectMaxMs",
                "reconnectInitialMs must be >= 50 and <= reconnectMaxMs (maximum 300000)",
            ));
        }
        if !(100..=60000).contains(&self.auth_timeout_ms) {
            return Err(invalid(
                "authTimeoutMs",
                "authTimeoutMs must be between 100 and 60000",
            ));
        }
        if !(1000..=3_600_000).contains(&self.task_message_sync_interval_ms) {
            return Err(invalid(
                "taskMessageSyncIntervalMs",
                "taskMessageSyncIntervalMs must be between 1000 and 3600000",
            ));
        }
        if self.task_overdue_minutes > 10_080 {
            return Err(invalid(
                "taskOverdueMinutes",
                "taskOverdueMinutes must be between 0 (off) and 10080 (7 days)",
            ));
        }
        if self.local_port == 80 {
            return Err(invalid(
                "localPort",
                "localPort cannot be 80 because HTTP default port is rejected by MCP endpoint validation",
            ));
        }
        if self.local_port_pending && self.local_port != 0 {
            return Err(invalid(
                "localPort",
                "Pending port assignment requires localPort 0",
            ));
        }
        Ok(())
    }

    /// 파일에서 JSON 설정을 읽어 파싱하고 유효성을 검사합니다.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).context("Cannot read config; run devlink init first")?;
        // 보안상 사용자 입력 값이 포함될 수 있는 serde 에러 원문은 외부에 노출하지 않음
        let config: Self = serde_json::from_slice(&bytes).map_err(|_| {
            invalid(
                "$",
                "Invalid config JSON, unknown field or incorrect value type",
            )
        })?;
        config.validate()?;
        Ok(config)
    }
}

/// 설정 파일의 전체 절대 경로를 결정합니다.
/// - CLI 옵션으로 지정되지 않은 경우 기본값 `~/.devlink/config.json`을 사용합니다.
pub fn config_path(path: Option<PathBuf>) -> Result<PathBuf> {
    let path = match path {
        Some(path) => path,
        None => dirs::home_dir()
            .context("Cannot find home directory")?
            .join(".devlink/config.json"),
    };
    Ok(if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    })
}

/// 설정 파일이 위치한 상위 디렉터리 경로를 반환합니다.
pub fn directory(path: &Path) -> &Path {
    path.parent().expect("absolute config path has a parent")
}

/// 설정 파일과 분리된 내부 런타임 및 잠금 파일 디렉터리를 반환합니다.
pub fn state_directory(config_path: &Path) -> PathBuf {
    directory(config_path).join(".devlink-runtime")
}

/// 프로세스 간 상호 배제를 위한 파일 락을 획득합니다 (`fs2::FileExt::try_lock_exclusive`).
/// 중복 실행이나 동시 설정 수정을 방지합니다.
pub fn lock(path: &Path) -> Result<File> {
    try_lock(path)?.context("Another DevLink operation is running")
}

/// Distinguish lock contention from unsafe paths and I/O failures without parsing text.
pub fn try_lock(path: &Path) -> Result<Option<File>> {
    crate::credentials::ensure_safe_parent_directory(directory(path))?;
    crate::credentials::ensure_safe_target_if_exists(path)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .context("Cannot open lock file")?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(file)),
        Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
            Ok(None)
        }
        Err(error) => Err(error).context("Cannot lock DevLink state file"),
    }
}

/// JSON 데이터를 원자적(Atomic)으로 파일에 씁니다.
/// - 임시 파일(TempFile)에 먼저 기록 후 `persist`(rename)하여 도중에 손상되는 일을 방지합니다.
/// - Unix 환경에서는 권한을 `0o600`(소유자 읽기/쓰기 전용)으로 제한합니다.
pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let file = prepare_json(path, value, false)?;
    file.persist(path)
        .map_err(|_| anyhow::anyhow!("Cannot replace JSON file"))?;
    Ok(())
}

/// Called with control.lock held, after authenticating the broker on its bound port.
pub(crate) fn save_setup_port(path: &Path, port: u16) -> Result<()> {
    anyhow::ensure!(
        crate::credentials::ensure_safe_target_if_exists(path)?,
        "Setup configuration is missing"
    );
    crate::credentials::verify_trusted_writer_permissions(path)?;
    let mut config = Config::load(path)?;
    if config.local_port_pending {
        config.local_port = port;
        config.local_port_pending = false;
        config.validate()?;
        prepare_json(path, &config, true)?
            .persist(path)
            .map_err(|_| anyhow::anyhow!("Cannot save the assigned broker port"))?;
        #[cfg(unix)]
        File::open(directory(path))?.sync_all()?;
    }
    Ok(())
}

fn prepare_json(
    path: &Path,
    value: &impl Serialize,
    private: bool,
) -> Result<tempfile::NamedTempFile> {
    let mut file =
        tempfile::NamedTempFile::new_in(directory(path)).context("Cannot create temporary file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    if private {
        crate::credentials::set_private_permissions(file.path())?;
        crate::credentials::verify_private_permissions(file.path())?;
    }
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    Ok(file)
}

#[derive(Debug)]
pub(crate) struct InitializeError {
    source: anyhow::Error,
    /// Only a failure after persist can leave the config publication uncertain.
    pub published: bool,
}

impl From<anyhow::Error> for InitializeError {
    fn from(source: anyhow::Error) -> Self {
        Self {
            source,
            published: false,
        }
    }
}
impl std::fmt::Display for InitializeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.source, f)
    }
}
impl std::error::Error for InitializeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Shared init implementation. Caller holds control.lock; never clobber a raced file.
/// Setup validates links separately; init preserves its existing symlink behavior.
pub(crate) fn initialize_missing(
    path: &Path,
    private: bool,
) -> std::result::Result<bool, InitializeError> {
    initialize_config_missing(path, private, &Config::default())
}

pub(crate) fn initialize_config_missing(
    path: &Path,
    private: bool,
    config: &Config,
) -> std::result::Result<bool, InitializeError> {
    initialize_missing_with_sync(path, private, config, |parent| {
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        #[cfg(not(unix))]
        let _ = parent;
        Ok(())
    })
}

fn initialize_missing_with_sync(
    path: &Path,
    private: bool,
    config: &Config,
    sync_parent: impl FnOnce(&Path) -> Result<()>,
) -> std::result::Result<bool, InitializeError> {
    // Any entry, including a dangling link, is an existing config. Propagate
    // inspection errors instead of treating them as a missing file.
    match fs::symlink_metadata(path) {
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(anyhow::Error::from(error)
                .context("Cannot inspect config")
                .into());
        }
    }
    let file = prepare_json(path, config, private)?;
    match file.persist_noclobber(path) {
        Ok(_) => {
            sync_parent(directory(path)).map_err(|source| InitializeError {
                source,
                published: true,
            })?;
            Ok(true)
        }
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(_) => Err(anyhow::anyhow!("Cannot create configuration file").into()),
    }
}

/// 설정 디렉터리와 기본 설정 파일을 초기화합니다.
/// - 파일이 이미 존재하면 생성하지 않고 `Ok(false)`를 반환합니다.
/// - 새로 생성된 경우 `Ok(true)`를 반환합니다.
pub fn init(path: &Path) -> Result<bool> {
    fs::create_dir_all(directory(path)).context("Cannot create config directory")?;
    let _guard = lock(&state_directory(path).join("control.lock"))?;
    let created = initialize_missing(path, false)?;
    if !created {
        Config::load(path)?;
    }
    Ok(created)
}

/// 설정 파일의 특정 키-값(key-value)을 업데이트합니다.
/// - 파일 락을 획득한 후 안전하게 수정하며, 유효성 검사 통과 시 원자적으로 저장합니다.
pub fn set(path: &Path, key: &str, raw: &str) -> Result<()> {
    let _guard = lock(&state_directory(path).join("control.lock"))?;
    let bytes = fs::read(path).context("Cannot read config; run devlink init first")?;
    let existing: Config = serde_json::from_slice(&bytes).map_err(|_| {
        anyhow::anyhow!("Invalid config JSON, unknown field or incorrect value type")
    })?;
    anyhow::ensure!(key != "localPortPending", "Unknown config key");
    let mut value = serde_json::to_value(existing)?;
    let field = value.get_mut(key).context("Unknown config key")?;
    *field = if field.is_string() {
        raw.into()
    } else {
        serde_json::from_str(raw).map_err(|_| anyhow::anyhow!("Invalid JSON config value"))?
    };
    let mut config: Config = serde_json::from_value(value)
        .map_err(|_| anyhow::anyhow!("Incorrect config value type or range"))?;
    if key == "localPort" {
        config.local_port_pending = false;
    }
    config.validate()?;
    atomic_json(path, &config)
}

#[cfg(test)]
mod tests {
    #[test]
    fn dependency_remote_validates_names_without_replacing_the_saved_config_on_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        atomic_json(&path, &Config::default()).unwrap();
        for valid in ["", "origin", "team.remote_2", &"a".repeat(64)] {
            set(&path, "dependencyRemote", valid).unwrap();
            assert_eq!(Config::load(&path).unwrap().dependency_remote, valid);
        }
        let saved = std::fs::read(&path).unwrap();
        for invalid in [
            "-option",
            "two words",
            "team/origin",
            "원격",
            &"a".repeat(65),
        ] {
            let error = set(&path, "dependencyRemote", invalid).unwrap_err();
            assert!(
                error.downcast_ref::<ConfigValidationError>().is_some(),
                "{error}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), saved);
        }
    }

    #[test]
    fn server_url_allows_cleartext_ws_only_on_loopback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        atomic_json(&path, &Config::default()).unwrap();
        for allowed in [
            "wss://api.devlink.ai.kr",
            "wss://central.example.test/ws/broker",
            "ws://localhost:8080",
            "ws://LOCALHOST:8080/",
            "ws://127.0.0.1:8080/ws/broker",
            "ws://[::1]:8080",
        ] {
            set(&path, "serverUrl", allowed).unwrap();
            assert_eq!(Config::load(&path).unwrap().server_url, allowed);
        }
        let saved = std::fs::read(&path).unwrap();
        for remote in [
            "ws://central.example.test",
            "ws://192.168.0.10:8080",
            "ws://desktop-ms327cl.mshome.net:8080/",
            "ws://127.0.0.1.example.test",
            "ws://localhost.example.test",
        ] {
            let error = set(&path, "serverUrl", remote).unwrap_err();
            let error = error.downcast_ref::<ConfigValidationError>().unwrap();
            assert_eq!(error.field, "serverUrl", "{remote}");
            assert!(error.message.contains("wss://"), "{remote}");
            assert_eq!(std::fs::read(&path).unwrap(), saved);
        }
    }

    #[test]
    fn task_overdue_minutes_accepts_zero_to_one_week() {
        for (minutes, ok) in [(0, true), (30, true), (10_080, true), (10_081, false)] {
            let config = Config {
                task_overdue_minutes: minutes,
                ..Config::default()
            };
            assert_eq!(config.validate().is_ok(), ok, "{minutes}");
        }
        assert_eq!(Config::default().task_overdue_minutes, 30);
    }

    #[test]
    fn git_collection_defaults_on_for_old_files_and_can_be_turned_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        // A config written before the setting existed keeps collecting.
        std::fs::write(
            &path,
            r#"{"serverUrl":"wss://api.devlink.ai.kr","agent":"codex","autoReconnect":true}"#,
        )
        .unwrap();
        assert!(Config::load(&path).unwrap().git_collection);
        assert!(Config::default().git_collection);

        set(&path, "gitCollection", "false").unwrap();
        assert!(!Config::load(&path).unwrap().git_collection);
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["gitCollection"], false);
        assert!(set(&path, "gitCollection", "\"no\"").is_err());
        set(&path, "gitCollection", "true").unwrap();
        assert!(Config::load(&path).unwrap().git_collection);
    }

    use super::*;

    #[test]
    fn initialization_failure_before_persist_does_not_publish_config() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("missing/config.json");
        let error = initialize_missing(&path, false).unwrap_err();
        assert!(!error.published);
        assert!(!path.exists());
    }

    #[test]
    fn parent_sync_failure_reports_the_already_persisted_config() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.json");
        let error = initialize_missing_with_sync(&path, false, &Config::default(), |_| {
            anyhow::bail!("injected parent sync failure")
        })
        .unwrap_err();
        assert!(error.published);
        Config::load(&path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn dangling_link_is_an_existing_entry_and_its_target_is_not_created() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("missing-target.json");
        let path = root.path().join("config.json");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(!initialize_missing(&path, false).unwrap());
        assert!(!target.exists());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }
}
