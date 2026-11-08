//! User session REST credentials, separate from Agent and local MCP tokens.
use crate::{
    config::{self, Config},
    credentials, private_file,
};
use anyhow::{Result, bail, ensure};
use reqwest::{
    Client, Method, Url,
    cookie::{CookieStore, Jar},
    header::{HeaderName, HeaderValue},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

const LIMIT: usize = 16_384;
const RESPONSE_LIMIT: usize = 262_144;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub user_id: u64,
    pub email: String,
    pub name: String,
    pub nickname: Option<String>,
    pub created_at: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum SessionState {
    Authenticated { user: User, cookie: String },
    LoggedOut,
    Expired,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Snapshot {
    schema_version: u32,
    origin: String,
    state: SessionState,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Csrf {
    header_name: String,
    token: String,
}
/// One-use prepared mutation credential; no Debug or serialization exposes it.
pub(crate) struct PreparedPost(Csrf);

#[derive(Deserialize)]
struct Envelope {
    success: bool,
    data: Option<Value>,
    code: Option<String>,
}

#[derive(Debug)]
pub struct ApiRejection {
    pub status: u16,
    pub code: &'static str,
}
impl std::fmt::Display for ApiRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.code == "USER_SESSION_EXPIRED" {
            write!(f, "USER_SESSION_EXPIRED: run devlink auth login (HTTP 401)")
        } else {
            write!(
                f,
                "{}: central request rejected (HTTP {})",
                self.code, self.status
            )
        }
    }
}
impl std::error::Error for ApiRejection {}
pub fn rejection_status(error: &anyhow::Error) -> Option<u16> {
    error.downcast_ref::<ApiRejection>().map(|e| e.status)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthFaultKind {
    Unavailable,
    Protocol,
    IdentityMismatch,
    Busy,
    ConfigBusy,
    BrokerBusy,
    ConfigInvalid,
    /// config.json could not be read; storage, not content, failed.
    ConfigUnreadable,
    /// control.lock or broker.lock is unsafe or could not be opened/locked.
    ControlLockFailed,
    BrokerLockFailed,
    SessionConflict,
}

#[derive(Debug)]
pub struct AuthFault {
    pub kind: AuthFaultKind,
    message: String,
}

impl std::fmt::Display for AuthFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for AuthFault {}

pub(crate) fn fault(kind: AuthFaultKind, message: &'static str) -> anyhow::Error {
    AuthFault {
        kind,
        message: message.into(),
    }
    .into()
}

/// Type a local failure for setup while keeping its value-free top-level message.
pub(crate) fn classify(error: anyhow::Error, kind: AuthFaultKind) -> anyhow::Error {
    let message = error.to_string();
    error.context(AuthFault { kind, message })
}

/// Public state only. Cookies and CSRF credentials stay inside UserClient.
pub enum SessionCheck {
    Authenticated(User),
    LoginRequired { expired_now: bool },
    OriginMismatch,
    InvalidLocalSession,
}

pub struct LoginResult {
    pub user: User,
    pub reused: bool,
}

#[derive(Debug)]
struct SessionInvalidated;
impl std::fmt::Display for SessionInvalidated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The previous user session was confirmed expired")
    }
}
pub fn session_was_invalidated(error: &anyhow::Error) -> bool {
    error.downcast_ref::<SessionInvalidated>().is_some()
}

/// A locked, authenticated user session. No Debug implementation exposes cookies.
pub struct UserClient {
    client: Client,
    jar: Arc<Jar>,
    origin: Url,
    snapshot: Snapshot,
    path: PathBuf,
    _lock: File,
    local_problem: Option<&'static str>,
}

pub fn session_path(config_path: &Path) -> PathBuf {
    config::state_directory(config_path).join("user-session.json")
}

/// Local setup continuity only; this does not replace a live central identity check.
pub(crate) fn setup_session_matches(
    config_path: &Path,
    origin: &str,
    user_id: u64,
) -> Result<bool> {
    Ok(private_file::read::<Snapshot>(&session_path(config_path), LIMIT)?.is_some_and(|s|
        s.schema_version == 1 && s.origin == origin && matches!(s.state,
            SessionState::Authenticated { user, cookie } if user.user_id == user_id && valid_user(&user) && valid_cookie(&cookie))))
}

pub fn validate_config(config_path: &Path) -> Result<()> {
    config_origin(config_path).map(|_| ())
}

pub(crate) fn config_origin(config_path: &Path) -> Result<Url> {
    Config::load(config_path)
        .and_then(|config| origin(&config))
        .map_err(|error| {
            // Only Config::load's read fails with io::Error; parse and value errors
            // carry ConfigValidationError with the field to report.
            let kind = if error.chain().any(|cause| cause.is::<std::io::Error>()) {
                AuthFaultKind::ConfigUnreadable
            } else {
                AuthFaultKind::ConfigInvalid
            };
            classify(error, kind)
        })
}

pub fn origin(config: &Config) -> Result<Url> {
    config.validate()?;
    let mut url = Url::parse(&config.server_url)
        .map_err(|_| config::invalid("serverUrl", "Invalid central origin"))?;
    let scheme = if url.scheme() == "wss" {
        "https"
    } else {
        "http"
    };
    url.set_scheme(scheme)
        .map_err(|_| config::invalid("serverUrl", "Invalid central origin"))?;
    url.set_path("/");
    let local = url
        .host_str()
        .is_some_and(|h| matches!(h, "localhost" | "127.0.0.1" | "[::1]" | "::1"));
    if url.scheme() != "https" && !local {
        return Err(config::invalid(
            "serverUrl",
            "User authentication requires HTTPS (HTTP is allowed only on loopback)",
        ));
    }
    Ok(url)
}

fn valid_cookie(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4096
        && value
            .bytes()
            .all(|b| b > 32 && b < 127 && !b";,\\\"".contains(&b))
}

fn valid_user(user: &User) -> bool {
    user.user_id > 0 && user.user_id <= i64::MAX as u64 && !user.email.is_empty()
}

fn transport(jar: Arc<Jar>) -> Result<Client> {
    Client::builder()
        .cookie_provider(jar)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|_| anyhow::anyhow!("Cannot initialize central user client"))
}

impl UserClient {
    fn load(config_path: &Path) -> Result<Self> {
        let origin = config_origin(config_path)?;
        let parent = config::state_directory(config_path);
        credentials::ensure_safe_parent_directory(&parent)?;
        credentials::set_private_permissions(&parent)?;
        let lock_path = parent.join("account.lock");
        if std::fs::symlink_metadata(&lock_path).is_ok() {
            credentials::verify_path_safety(&lock_path)?;
        }
        let lock = config::try_lock(&lock_path)?
            .ok_or_else(|| fault(AuthFaultKind::Busy, "Another DevLink operation is running"))?;
        let path = session_path(config_path);
        let empty = || Snapshot {
            schema_version: 1,
            origin: origin.to_string(),
            state: SessionState::LoggedOut,
        };
        // Only JSON/schema/origin problems are recoverable. Path/ACL/size failures
        // from read_bytes stay fatal and never authorize overwriting unsafe files.
        let (snapshot, local_problem) = match private_file::read_bytes(&path, LIMIT)? {
            None => (empty(), None),
            Some(bytes) => match serde_json::from_slice::<Snapshot>(&bytes) {
                Ok(value) if value.schema_version == 1 => {
                    if value.origin != origin.as_str() {
                        (empty(), Some("origin_mismatch"))
                    } else if matches!(&value.state, SessionState::Authenticated{user,cookie} if !valid_user(user) || !valid_cookie(cookie))
                    {
                        (empty(), Some("invalid_local_session"))
                    } else {
                        (value, None)
                    }
                }
                _ => (empty(), Some("invalid_local_session")),
            },
        };
        let jar = Arc::new(Jar::default());
        if let SessionState::Authenticated { user, cookie } = &snapshot.state {
            ensure!(
                valid_user(user) && valid_cookie(cookie),
                "Invalid private user session"
            );
            let secure = if origin.scheme() == "https" {
                "; Secure"
            } else {
                ""
            };
            jar.add_cookie_str(
                &format!("JSESSIONID={cookie}; Path=/; HttpOnly{secure}"),
                &origin,
            );
        }
        let client = transport(jar.clone())?;
        Ok(Self {
            client,
            jar,
            origin,
            snapshot,
            path,
            _lock: lock,
            local_problem,
        })
    }

    pub async fn authenticated(config_path: &Path) -> Result<Self> {
        let mut this = Self::load(config_path)?;
        this.verify_session().await?;
        Ok(this)
    }

    /// Pin the setup principal before sending requests, including after UI input.
    pub async fn authenticated_for_setup(
        config_path: &Path,
        expected_origin: &str,
        expected_user: u64,
    ) -> Result<Self> {
        let mut this = Self::load(config_path)?;
        require_setup_origin(&this, expected_origin)?;
        if this.local_problem.is_some()
            || !matches!(&this.snapshot.state, SessionState::Authenticated { user, .. } if user.user_id == expected_user)
        {
            return Err(fault(
                AuthFaultKind::SessionConflict,
                "The setup user session changed; restart setup",
            ));
        }
        this.verify_session().await?;
        Ok(this)
    }

    async fn verify_session(&mut self) -> Result<()> {
        ensure!(
            self.local_problem.is_none(),
            "Local user session cannot be used; run devlink auth login or auth logout"
        );
        let old_user = match &self.snapshot.state {
            SessionState::Authenticated { user, .. } => user.user_id,
            SessionState::Expired => bail!("USER_SESSION_EXPIRED: run devlink auth login"),
            SessionState::LoggedOut => bail!("USER_LOGIN_REQUIRED: run devlink auth login"),
        };
        let user: User = self
            .exchange(Method::GET, "/api/v1/auth/users/me", None, None)
            .await?;
        if !valid_user(&user) || user.user_id != old_user {
            self.snapshot.state = SessionState::Expired;
            self.save()?;
            return Err(fault(
                AuthFaultKind::IdentityMismatch,
                "Central user identity changed; sign in again",
            ));
        }
        self.snapshot.state = SessionState::Authenticated {
            user,
            cookie: self.cookie()?,
        };
        self.save()?;
        Ok(())
    }

    pub fn user(&self) -> Result<&User> {
        match &self.snapshot.state {
            SessionState::Authenticated { user, .. } => Ok(user),
            _ => bail!("USER_LOGIN_REQUIRED: run devlink auth login"),
        }
    }

    pub fn central_origin(&self) -> &str {
        self.origin.as_str()
    }

    fn cookie(&self) -> Result<String> {
        let header = self
            .jar
            .cookies(&self.origin)
            .ok_or_else(|| fault(AuthFaultKind::Protocol, "Central session cookie missing"))?;
        let text = header
            .to_str()
            .map_err(|_| fault(AuthFaultKind::Protocol, "Invalid central cookie"))?;
        let cookie = text
            .split(';')
            .map(str::trim)
            .find_map(|part| part.strip_prefix("JSESSIONID="))
            .filter(|value| valid_cookie(value))
            .ok_or_else(|| fault(AuthFaultKind::Protocol, "Central session cookie missing"))?;
        Ok(cookie.to_owned())
    }

    fn save(&self) -> Result<()> {
        private_file::write(&self.path, &self.snapshot, LIMIT)
    }

    async fn csrf(&mut self) -> Result<Csrf> {
        let csrf: Csrf = self
            .exchange(Method::GET, "/api/v1/auth/csrf", None, None)
            .await?;
        if !(matches!(csrf.header_name.as_str(), "X-CSRF-TOKEN" | "X-XSRF-TOKEN")
            && !csrf.token.is_empty()
            && csrf.token.len() <= 8192)
        {
            return Err(fault(
                AuthFaultKind::Protocol,
                "Invalid central CSRF response",
            ));
        }
        Ok(csrf)
    }

    pub async fn get<T: DeserializeOwned>(&mut self, path: &str) -> Result<T> {
        self.exchange(Method::GET, path, None, None).await
    }

    pub(crate) async fn get_with_query<T: DeserializeOwned>(
        &mut self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T> {
        self.exchange_with_query(Method::GET, path, None, None, query)
            .await
    }

    pub async fn post<T: DeserializeOwned>(&mut self, path: &str, body: Value) -> Result<T> {
        let prepared = self.prepare_post().await?;
        self.post_prepared(path, body, prepared).await
    }

    pub(crate) async fn prepare_post(&mut self) -> Result<PreparedPost> {
        self.csrf().await.map(PreparedPost)
    }

    pub(crate) async fn post_prepared<T: DeserializeOwned>(
        &mut self,
        path: &str,
        body: Value,
        prepared: PreparedPost,
    ) -> Result<T> {
        self.exchange(Method::POST, path, Some(body), Some(prepared.0))
            .await
    }

    pub(crate) async fn prepare_patch(&mut self) -> Result<PreparedPost> {
        self.csrf().await.map(PreparedPost)
    }

    /// No preflight IO remains: callers may fence their session after preparing CSRF.
    pub(crate) async fn patch_prepared<T: DeserializeOwned>(
        &mut self,
        path: &str,
        body: Value,
        prepared: PreparedPost,
    ) -> Result<T> {
        self.exchange(Method::PATCH, path, Some(body), Some(prepared.0))
            .await
    }

    async fn exchange<T: DeserializeOwned>(
        &mut self,
        method: Method,
        path: &str,
        body: Option<Value>,
        csrf: Option<Csrf>,
    ) -> Result<T> {
        self.exchange_with_query(method, path, body, csrf, &[])
            .await
    }

    async fn exchange_with_query<T: DeserializeOwned>(
        &mut self,
        method: Method,
        path: &str,
        body: Option<Value>,
        csrf: Option<Csrf>,
        query: &[(&str, &str)],
    ) -> Result<T> {
        ensure!(
            path.starts_with("/api/v1/")
                && !path.contains(['?', '#', '\\'])
                && !path.split('/').any(|part| matches!(part, "." | "..")),
            "Invalid central API path"
        );
        let mut url = self.origin.clone();
        url.set_path(path);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query.iter().copied());
        }
        let mut request = self.client.request(method, url);
        if let Some(body) = body {
            request = request.json(&body);
        }
        if let Some(csrf) = csrf {
            let mut value = HeaderValue::from_str(&csrf.token)
                .map_err(|_| fault(AuthFaultKind::Protocol, "Invalid CSRF token"))?;
            value.set_sensitive(true);
            let name = HeaderName::from_bytes(csrf.header_name.as_bytes())
                .map_err(|_| fault(AuthFaultKind::Protocol, "Invalid CSRF header"))?;
            request = request.header(name, value);
        }
        let mut response = request.send().await.map_err(|_| {
            fault(
                AuthFaultKind::Unavailable,
                "CENTRAL_UNAVAILABLE: request outcome may be unknown; no automatic retry",
            )
        })?;
        let status = response.status().as_u16();
        if status == 401 && matches!(self.snapshot.state, SessionState::Authenticated { .. }) {
            self.snapshot.state = SessionState::Expired;
            self.save()?;
            return Err(ApiRejection {
                status: 401,
                code: "USER_SESSION_EXPIRED",
            }
            .into());
        }
        if (300..400).contains(&status) {
            return Err(fault(
                AuthFaultKind::Protocol,
                "Central redirects are not supported",
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| {
            fault(
                AuthFaultKind::Unavailable,
                "Central response incomplete; request outcome may be unknown",
            )
        })? {
            if bytes.len() + chunk.len() > RESPONSE_LIMIT {
                return Err(fault(
                    AuthFaultKind::Protocol,
                    "Central response exceeds size limit",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        let envelope: Envelope = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(_) if !(200..300).contains(&status) => return Err(rejection(status, None)),
            Err(_) => {
                return Err(fault(
                    AuthFaultKind::Protocol,
                    "Invalid central response; request outcome may be unknown",
                ));
            }
        };
        if !(200..300).contains(&status) || !envelope.success {
            return Err(rejection(status, envelope.code.as_deref()));
        }
        serde_json::from_value(envelope.data.unwrap_or(Value::Null)).map_err(|_| {
            fault(
                AuthFaultKind::Protocol,
                "Invalid central response data; request outcome may be unknown",
            )
        })
    }
}

fn rejection(status: u16, code: Option<&str>) -> anyhow::Error {
    let label = match code {
        Some("INVALID_CREDENTIALS") => "INVALID_CREDENTIALS",
        Some("VALIDATION_FAILED") => "VALIDATION_FAILED",
        Some("ACCESS_DENIED") => "ACCESS_DENIED",
        Some("RESOURCE_NOT_FOUND") => "RESOURCE_NOT_FOUND",
        Some("AGENT_ALREADY_EXISTS") if status == 409 => "AGENT_ALREADY_EXISTS",
        _ => match status {
            401 => "AUTHENTICATION_REQUIRED",
            403 => "ACCESS_DENIED",
            404 => "RESOURCE_NOT_FOUND",
            409 => "CONFLICT",
            _ => "CENTRAL_REQUEST_REJECTED",
        },
    };
    ApiRejection {
        status,
        code: label,
    }
    .into()
}

fn validate_login_input(email: &str, password: &str) -> Result<()> {
    ensure!(
        !email.trim().is_empty()
            && email.len() <= 320
            && !password.is_empty()
            && password.len() <= 72,
        "Invalid login input (password limit: 72 UTF-8 bytes)"
    );
    Ok(())
}

pub async fn login(config_path: &Path, email: &str, password: &str) -> Result<User> {
    validate_login_input(email, password)?;
    login_with_client(UserClient::load(config_path)?, config_path, email, password).await
}

/// Recheck under the same account lock used for login. A session created while the
/// UI was collecting input wins; setup must not replace a now-valid session.
pub async fn login_for_setup(
    config_path: &Path,
    email: &str,
    password: &str,
    expected_origin: &str,
) -> Result<LoginResult> {
    let mut client = setup_login_client(config_path, expected_origin)?;
    let mut expired_now = false;
    if matches!(client.snapshot.state, SessionState::Authenticated { .. }) {
        match client.verify_session().await {
            Ok(()) => {
                return Ok(LoginResult {
                    user: client.user()?.clone(),
                    reused: true,
                });
            }
            Err(error) if rejection_status(&error) == Some(401) => expired_now = true,
            Err(error) => return Err(error),
        }
    }
    validate_login_input(email, password)?;
    login_with_client(client, config_path, email, password)
        .await
        .map(|user| LoginResult {
            user,
            reused: false,
        })
        .map_err(|error| {
            if expired_now {
                error.context(SessionInvalidated)
            } else {
                error
            }
        })
}

/// An explicit batch login always authenticates the supplied account.
pub async fn login_for_setup_explicit(
    config_path: &Path,
    email: &str,
    password: &str,
    expected_origin: &str,
) -> Result<User> {
    validate_login_input(email, password)?;
    let client = setup_login_client(config_path, expected_origin)?;
    login_with_client(client, config_path, email, password).await
}

fn setup_login_client(config_path: &Path, expected_origin: &str) -> Result<UserClient> {
    let client = UserClient::load(config_path)?;
    require_setup_origin(&client, expected_origin)?;
    if client.local_problem.is_some() {
        return Err(fault(
            AuthFaultKind::SessionConflict,
            "The local session changed; inspect it before signing in",
        ));
    }
    Ok(client)
}

async fn login_with_client(
    mut client: UserClient,
    config_path: &Path,
    email: &str,
    password: &str,
) -> Result<User> {
    // Start a new cookie jar: failed login must preserve an existing local session.
    client.jar = Arc::new(Jar::default());
    client.client = transport(client.jar.clone())?;
    client.snapshot.state = SessionState::LoggedOut;
    let csrf = client.csrf().await?;
    let user: User = client
        .exchange(
            Method::POST,
            "/api/v1/auth/login",
            Some(json!({"email":email,"password":password})),
            Some(csrf),
        )
        .await?;
    let completion = async {
        let verified: User = client
            .exchange(Method::GET, "/api/v1/auth/users/me", None, None)
            .await?;
        if !valid_user(&verified) || user.user_id != verified.user_id {
            return Err(fault(
                AuthFaultKind::IdentityMismatch,
                "Central login identity mismatch",
            ));
        }
        crate::agent_profile::before_login_publish(config_path, verified.user_id)?;
        client.csrf().await?; // Login rotates the session and the CSRF token.
        if config_origin(config_path)? != client.origin {
            return Err(fault(
                AuthFaultKind::SessionConflict,
                "The configured server changed during sign-in",
            ));
        }
        client.snapshot = Snapshot {
            schema_version: 1,
            origin: client.origin.to_string(),
            state: SessionState::Authenticated {
                user: verified.clone(),
                cookie: client.cookie()?,
            },
        };
        client.save()?;
        Ok(verified)
    }
    .await;
    if completion.is_err() {
        // The new server session is not usable locally. Cleanup must not overwrite
        // a previous local snapshot if this best-effort logout itself gets a 401.
        client.snapshot.state = SessionState::LoggedOut;
        let _ = client.post::<Value>("/api/v1/auth/logout", json!({})).await;
    }
    completion
}

pub async fn status(config_path: &Path) -> Result<Value> {
    let client = UserClient::load(config_path)?;
    if let Some(problem) = client.local_problem {
        return Ok(json!({"state":problem,"authenticated":false}));
    }
    match client.snapshot.state {
        SessionState::LoggedOut => Ok(json!({"state":"logged_out","authenticated":false})),
        SessionState::Expired => Ok(json!({"state":"expired","authenticated":false})),
        SessionState::Authenticated { .. } => {
            drop(client);
            let client = UserClient::authenticated(config_path).await?;
            Ok(json!({"state":"authenticated","authenticated":true,"user":client.user()?}))
        }
    }
}

/// Check once under the account lock. Only a confirmed 401 requests reauthentication;
/// transport, 5xx, protocol, and identity failures remain errors and preserve their class.
pub async fn check_session(config_path: &Path, expected_origin: &str) -> Result<SessionCheck> {
    let mut client = UserClient::load(config_path)?;
    require_setup_origin(&client, expected_origin)?;
    match client.local_problem {
        Some("origin_mismatch") => return Ok(SessionCheck::OriginMismatch),
        Some(_) => return Ok(SessionCheck::InvalidLocalSession),
        None => {}
    }
    if !matches!(client.snapshot.state, SessionState::Authenticated { .. }) {
        return Ok(SessionCheck::LoginRequired { expired_now: false });
    }
    match client.verify_session().await {
        Ok(()) => Ok(SessionCheck::Authenticated(client.user()?.clone())),
        Err(error) if rejection_status(&error) == Some(401) => {
            Ok(SessionCheck::LoginRequired { expired_now: true })
        }
        Err(error) => Err(error),
    }
}

fn require_setup_origin(client: &UserClient, expected_origin: &str) -> Result<()> {
    if client.central_origin() != expected_origin {
        return Err(fault(
            AuthFaultKind::SessionConflict,
            "The central origin changed during setup; inspect the profile and start setup again",
        ));
    }
    Ok(())
}

pub async fn logout(config_path: &Path) -> Result<bool> {
    let mut client = UserClient::load(config_path)?;
    let remote_attempted = matches!(client.snapshot.state, SessionState::Authenticated { .. });
    let result = if remote_attempted {
        client
            .post::<Value>("/api/v1/auth/logout", json!({}))
            .await
            .map(|_| ())
    } else {
        Ok(())
    };
    client.snapshot.state = SessionState::LoggedOut;
    client.save()?; // Clear local secrets even if the remote server cannot be reached.
    result
        .map(|_| remote_attempted)
        .map_err(|_| anyhow::anyhow!("Local user session cleared; central logout not confirmed"))
}
