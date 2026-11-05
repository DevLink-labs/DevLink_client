//! # DevLink MCP Credentials Management
//!
//! MCP(Model Context Protocol) 서버와 로컬 브로커 간의 비공개 연결 자격 파일(`broker.json`)을
//! 생성, 검증, 원자적 발행 및 회전(rotate)하는 모듈입니다.
//!
//! - `packages/mcp-server/src/broker/settings.ts`의 `settingsSchema`와 100% 호환됩니다.
//! - OS별 엄격한 비공개 파일 권한(Windows ACL, Unix 0600/0700)을 적용하고 검증합니다.
//! - 토큰 및 파일 내용은 로그, 오류, 표준 출력에 절대 노출되지 않습니다.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub const SCHEMA_VERSION: u32 = 1;
pub const CREDENTIALS_FILE_NAME: &str = "broker.json";
pub const MAX_CREDENTIALS_SIZE: u64 = 16_384;

/// Per-check readiness budget kept from the former child-process verifier; native
/// Windows ACL checks finish far sooner. Publishing changed credentials may verify
/// the existing snapshot, the temporary file, and the persisted file.
pub const ACL_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
pub const MAX_PUBLISH_ACL_CHECKS: u32 = 3;

/// Extra readiness allowance before the broker can publish runtime.json.
/// Keep the CLI's original startup/health margin in addition to these checks.
pub fn startup_validation_allowance() -> std::time::Duration {
    if cfg!(windows) {
        // Existing inbox and task request snapshots each add one ACL check.
        ACL_CHECK_TIMEOUT * (MAX_PUBLISH_ACL_CHECKS + 2)
    } else {
        std::time::Duration::ZERO
    }
}

/// MCP 서버용 비공개 브로커 자격 스냅샷
///
/// MCP settingsSchema:
/// ```json
/// {
///   "schemaVersion": 1,
///   "brokerUrl": "http://127.0.0.1:43127",
///   "localToken": "<secret-token>"
/// }
/// ```
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpCredentials {
    pub schema_version: u32,
    pub broker_url: String,
    pub local_token: String,
}

impl std::fmt::Debug for McpCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpCredentials")
            .field("schema_version", &self.schema_version)
            .field("broker_url", &self.broker_url)
            .field("local_token", &"[REDACTED]")
            .finish()
    }
}

/// 설정 파일 경로에 대응하는 MCP 자격 파일의 절대 경로를 반환합니다.
pub fn path(config_path: &Path) -> PathBuf {
    crate::config::state_directory(config_path).join(CREDENTIALS_FILE_NAME)
}

/// 암호학적으로 안전한 64바이트(hex) 로컬 토큰을 생성합니다.
///
/// 256비트 CSPRNG 엔트로피(UUID v4 2개 결합)를 사용하며,
/// `[a-f0-9]` 문자열로 구성되어 MCP 요구 정규식 `^[A-Za-z0-9._~-]+$` 및 16~4096자 제한을 충족합니다.
pub fn generate_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// 브로커 URL 형식 검증 (`http://127.0.0.1:<port>` 형식, 슬래시/경로/쿼리/사용자정보 금지)
/// - 포트는 선행 0이나 부호(`+`)가 없는 순수 ASCII 숫자(`[1-9][0-9]*`)여야 합니다.
/// - WHATWG URL 표준에서 `http:` 기본 포트인 80은 `url.port`가 빈 문자열로 정규화되어
///   MCP `validateSettings`가 거부하므로 포트 80은 허용하지 않습니다.
pub fn is_valid_broker_url(url: &str) -> bool {
    if let Some(port_str) = url.strip_prefix("http://127.0.0.1:")
        && !port_str.is_empty()
        && !port_str.starts_with('0')
        && port_str.bytes().all(|b| b.is_ascii_digit())
        && let Ok(port) = port_str.parse::<u16>()
    {
        return port > 0 && port != 80;
    }
    false
}

/// 로컬 토큰 형식 검증 (16~4096자, `[A-Za-z0-9._~-]` 집합)
pub fn is_valid_local_token(token: &str) -> bool {
    token.len() >= 16
        && token.len() <= 4096
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '-'))
}

/// MCP 자격 데이터 구조 검증
pub fn validate(creds: &McpCredentials) -> Result<()> {
    ensure!(
        creds.schema_version == SCHEMA_VERSION,
        "Unsupported schemaVersion in MCP credentials"
    );
    ensure!(
        is_valid_broker_url(&creds.broker_url),
        "Invalid brokerUrl in MCP credentials; must be http://127.0.0.1:<port>"
    );
    ensure!(
        is_valid_local_token(&creds.local_token),
        "Invalid localToken format or length in MCP credentials"
    );
    Ok(())
}

/// 자격 파일 대상이 이미 존재하는 경우 일반 파일인지(심볼릭 링크·reparse point·디렉터리 아님) 확인합니다.
pub(crate) fn ensure_safe_target_if_exists(target: &Path) -> Result<bool> {
    match fs::symlink_metadata(target) {
        Ok(meta) => {
            ensure!(
                !meta.is_symlink(),
                "Credential file must not be a symbolic link"
            );
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x00000400;
                ensure!(
                    (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) == 0,
                    "Credential file must not be a reparse point"
                );
            }
            ensure!(meta.is_file(), "Credential path must be a regular file");
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).context("Cannot inspect credential file"),
    }
}

/// 자격 파일 및 부모 디렉터리의 경로 안전성 검증
/// - 일반 파일 여부, 심볼릭 링크 거부, 16KiB 이하 크기 확인
/// - 부모 디렉터리 존재 및 심볼릭 링크 거부 확인
pub fn verify_path_safety(file_path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(file_path).context("Cannot inspect credential file")?;
    ensure!(
        !meta.is_symlink(),
        "Credential file must not be a symbolic link"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x00000400;
        ensure!(
            (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) == 0,
            "Credential file must not be a reparse point"
        );
    }
    ensure!(meta.is_file(), "Credential path must be a regular file");
    ensure!(
        meta.len() <= MAX_CREDENTIALS_SIZE,
        "Credential file exceeds maximum size limit (16KiB)"
    );

    let parent = file_path
        .parent()
        .context("Credential file has no parent directory")?;
    verify_parent_safety(parent)?;
    Ok(())
}

/// 부모 디렉터리 경로의 안전성을 검증합니다.
/// - 일반 디렉터리 여부 확인
/// - 심볼릭 링크 거부
/// - Windows junction 등 reparse point 거부
/// - 메타데이터 조회 오류를 경로 없음으로 간주하지 않고 즉시 반환
pub fn verify_parent_safety(parent: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(parent).with_context(|| {
        format!(
            "Cannot inspect credential parent directory at '{}'",
            parent.display()
        )
    })?;

    ensure!(
        !meta.is_symlink(),
        "Credential parent directory must not be a symbolic link: '{}'",
        parent.display()
    );

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x00000400;
        ensure!(
            (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) == 0,
            "Credential parent directory must not be a reparse point or junction: '{}'",
            parent.display()
        );
    }

    ensure!(
        meta.is_dir(),
        "Credential parent path must be a directory: '{}'",
        parent.display()
    );

    Ok(())
}

/// 부모 디렉터리를 안전하게 준비하고 검증합니다.
/// - 존재하는 부모는 생성/권한 변경/기존 자격 읽기 전에 검사합니다.
/// - 부모를 새로 만들었다면 사용 전에 다시 검사합니다.
/// - 검사 오류를 경로 없음으로 간주하지 않고 즉시 반환합니다.
pub fn ensure_safe_parent_directory(parent: &Path) -> Result<()> {
    match fs::symlink_metadata(parent) {
        Ok(_) => {
            // 이미 존재하는 부모 경로는 생성·권한 변경·기존 자격 읽기 전에 검사
            verify_parent_safety(parent)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // 부모 디렉터리가 없으면 생성 후 사용 전에 다시 검사
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Cannot create credential parent directory at '{}'",
                    parent.display()
                )
            })?;
            verify_parent_safety(parent)?;
        }
        Err(e) => {
            // 검사 오류를 경로 없음으로 간주하지 않음
            return Err(e).with_context(|| {
                format!(
                    "Cannot inspect credential parent directory at '{}'",
                    parent.display()
                )
            });
        }
    }
    Ok(())
}

#[cfg(windows)]
pub fn current_user_sid() -> Result<String> {
    Ok(crate::win_acl::current_user()?.text.clone())
}

/// Replaces the DACL with a protected one that grants only the current user full
/// control (inheritable for directories). Unlike the former `icacls /grant:r`, other
/// explicit ACEs are removed too.
#[cfg(windows)]
pub fn set_private_permissions(path: &Path) -> Result<()> {
    crate::win_acl::set_private(path, path.is_dir()).with_context(|| {
        format!(
            "Failed to set private Windows ACL on path: {}",
            path.display()
        )
    })
}

#[cfg(windows)]
pub fn verify_private_permissions(path: &Path) -> Result<()> {
    verify_permissions(path, false, false, false, false)
}

/// Non-secret files that still steer authentication (config.json): others may read,
/// but only the current user and trusted principals may write, delete or re-ACL them.
/// On Windows, the owner may also be BUILTIN\Administrators for elevated init.
pub fn verify_trusted_writer_permissions(path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        verify_permissions(path, false, true, false, true)
    }
    #[cfg(unix)]
    {
        verify_permissions(path, true)
    }
}

// Former PowerShell verifier, kept as the reference the native policy below must
// match. Tests evaluate it on .NET ACL objects so owner cases need no elevation.
#[cfg(all(windows, test))]
const WINDOWS_ACL_POLICY: &str = r#"
function Get-DevLinkAclFailure($acl, [string]$sid, [bool]$isFile, [bool]$othersMayRead, [bool]$source = $false, [bool]$checkOwner = $isFile, [bool]$configFile = $false) {
  $trusted = @($sid, 'S-1-5-18', 'S-1-5-32-544')
  $owners = @($sid)
  if ($source) {
    $owners = $trusted
    try { $owners += ([System.Security.Principal.NTAccount]'NT SERVICE\TrustedInstaller').Translate([System.Security.Principal.SecurityIdentifier]).Value } catch {}
    $trusted = $owners
  } elseif ($configFile -and $othersMayRead) { $owners += 'S-1-5-32-544' }
  if ($checkOwner) {
    $owner = $acl.GetOwner([System.Security.Principal.SecurityIdentifier]).Value
    if ($owner -notin $owners) { return 2 }
  }
  foreach ($rule in $acl.GetAccessRules($true, $true, [System.Security.Principal.SecurityIdentifier])) {
    if ($othersMayRead -and $rule.IdentityReference.Value -in @('S-1-3-0','S-1-3-1') -and (($rule.PropagationFlags -band [System.Security.AccessControl.PropagationFlags]::InheritOnly) -ne 0)) { continue }
    if ($rule.AccessControlType -eq 'Allow' -and $rule.IdentityReference.Value -notin $trusted) {
      if (($isFile -and -not $othersMayRead) -or (($rule.FileSystemRights -band 852310) -ne 0)) { return 3 }
    }
  }
  return 0
}
"#;

#[cfg(windows)]
const SYSTEM_SID: &str = "S-1-5-18";
#[cfg(windows)]
const ADMINISTRATORS_SID: &str = "S-1-5-32-544";
/// Service SIDs are derived from the service name, so this value is fixed.
#[cfg(windows)]
const TRUSTED_INSTALLER_SID: &str =
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";
/// WriteData, AppendData, WriteEA, DeleteChild, WriteAttributes, Delete, WriteDac, WriteOwner.
#[cfg(windows)]
const FORBIDDEN_WRITE_MASK: u32 = 852_310;

#[cfg(windows)]
#[derive(Clone, Copy, Debug)]
struct AclMode {
    is_file: bool,
    others_may_read: bool,
    source: bool,
    check_owner: bool,
    config_file: bool,
}

/// Native port of `Get-DevLinkAclFailure`: 0 allowed, 2 untrusted owner, 3 untrusted
/// access. ACE filtering mirrors .NET `CommonAcl`, which drops zero-mask ACEs,
/// inherit-only ACEs on files, and inherit-only ACEs that propagate nowhere.
#[cfg(windows)]
fn acl_failure(descriptor: &crate::win_acl::Descriptor, sid: &str, mode: AclMode) -> u8 {
    const OBJECT_INHERIT: u8 = 0x1;
    const CONTAINER_INHERIT: u8 = 0x2;
    const INHERIT_ONLY: u8 = 0x8;
    let mut trusted = vec![sid, SYSTEM_SID, ADMINISTRATORS_SID];
    let mut owners = vec![sid];
    if mode.source {
        trusted.push(TRUSTED_INSTALLER_SID);
        owners = trusted.clone();
    } else if mode.config_file && mode.others_may_read {
        owners.push(ADMINISTRATORS_SID);
    }
    let listed =
        |list: &[&str], value: &str| list.iter().any(|item| item.eq_ignore_ascii_case(value));
    if mode.check_owner
        && !descriptor
            .owner
            .as_deref()
            .is_some_and(|owner| listed(&owners, owner))
    {
        return 2;
    }
    // .NET reads a null DACL as Everyone: FullControl.
    let Some(allows) = &descriptor.allows else {
        return 3;
    };
    for allow in allows {
        let inherit_only = allow.flags & INHERIT_ONLY != 0;
        let propagates = allow.flags & (OBJECT_INHERIT | CONTAINER_INHERIT) != 0;
        if allow.mask == 0 || (inherit_only && (mode.is_file || !propagates)) {
            continue;
        }
        let sid = allow.sid.as_deref();
        if mode.others_may_read
            && inherit_only
            && sid.is_some_and(|sid| listed(&["S-1-3-0", "S-1-3-1"], sid))
        {
            continue;
        }
        if sid.is_some_and(|sid| listed(&trusted, sid)) {
            continue;
        }
        if (mode.is_file && !mode.others_may_read) || allow.mask & FORBIDDEN_WRITE_MASK != 0 {
            return 3;
        }
    }
    0
}

#[cfg(windows)]
pub fn verify_directory_permissions(path: &Path) -> Result<()> {
    verify_parent_safety(path)?;
    verify_permissions(path, true, true, false, false)
}

/// 비공개 파일은 packages/mcp-server/src/broker/settings.ts와 동일한 기준 적용:
/// - 비공개 파일 소유자는 현재 사용자 SID; config는 Administrators도 허용
/// - 파일은 현재 사용자, SYSTEM(S-1-5-18), BUILTIN\Administrators(S-1-5-32-544) 외 Allow 거부
/// - 직계 부모 디렉터리는 위 신뢰 주체 외 금지된 쓰기·삭제·권한 변경(852310 마스크) Allow 거부
/// - others_may_read이면 파일에도 부모와 같은 852310 마스크만 적용해 읽기 Allow는 허용
/// - source/state 모드만 SYSTEM/TrustedInstaller 소유를 허용하며 config 예외와 분리
#[cfg(windows)]
fn verify_permissions(
    path: &Path,
    directory: bool,
    public_read: bool,
    source: bool,
    config_file: bool,
) -> Result<()> {
    let insecure = || {
        anyhow::anyhow!(
            "Insecure or inaccessible Windows permissions on {}",
            path.display()
        )
    };
    let abs_path = std::path::absolute(path)
        .with_context(|| format!("Cannot determine absolute path for {}", path.display()))?;
    let sid = current_user_sid()?;
    let mut targets = vec![abs_path.as_path()];
    if !directory {
        targets.push(abs_path.parent().ok_or_else(insecure)?);
    }
    for (index, target) in targets.into_iter().enumerate() {
        let primary = index == 0;
        let descriptor = crate::win_acl::read(target).map_err(|_| insecure())?;
        let mode = AclMode {
            is_file: primary && !directory,
            others_may_read: public_read,
            source,
            check_owner: primary,
            config_file,
        };
        if acl_failure(&descriptor, &sid, mode) != 0 {
            return Err(insecure());
        }
    }
    Ok(())
}

#[cfg(unix)]
pub fn set_private_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if path.is_dir() {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    } else {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(unix)]
pub fn verify_private_permissions(path: &Path) -> Result<()> {
    verify_permissions(path, false)
}

#[cfg(unix)]
fn verify_permissions(path: &Path, others_may_read: bool) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let info = fs::symlink_metadata(path)?;
    let parent = path.parent().context("no parent directory")?;
    let parent_info = fs::symlink_metadata(parent)?;

    let uid = unsafe { libc::getuid() };
    if info.uid() != uid {
        bail!("Credential file is not owned by current user");
    }
    if (info.mode() & if others_may_read { 0o022 } else { 0o077 }) != 0 {
        bail!("Credential file has group/other permissions");
    }
    if (parent_info.mode() & 0o022) != 0 {
        bail!("Credential parent directory is writable by group/other");
    }
    Ok(())
}

#[cfg(unix)]
pub fn verify_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    verify_parent_safety(path)?;
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.uid() == unsafe { libc::getuid() } && meta.mode() & 0o022 == 0,
        "Unsafe project directory permissions"
    );
    Ok(())
}

/// State ancestors may be owned by the operating system (for example a Windows home).
#[cfg(windows)]
pub(crate) fn verify_state_directory_permissions(path: &Path) -> Result<()> {
    verify_parent_safety(path)?;
    verify_permissions(path, true, true, true, false)
}
#[cfg(unix)]
pub(crate) fn verify_state_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    verify_parent_safety(path)?;
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        (meta.uid() == unsafe { libc::getuid() } || meta.uid() == 0) && meta.mode() & 0o022 == 0,
        "Unsafe state directory"
    );
    Ok(())
}

/// Host configuration permits other readers; source binaries may be system-owned.
/// Credentials continue to use the stricter private-file verifier.
#[cfg(windows)]
pub(crate) fn verify_public_file_permissions(path: &Path, source: bool) -> Result<()> {
    verify_permissions(path, false, true, source, false)
}
#[cfg(unix)]
pub(crate) fn verify_public_file_permissions(path: &Path, source: bool) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let info = fs::symlink_metadata(path)?;
    let parent = fs::symlink_metadata(path.parent().context("No parent")?)?;
    let uid = unsafe { libc::getuid() };
    ensure!(
        info.uid() == uid || (source && info.uid() == 0),
        "Unsafe file owner"
    );
    ensure!(
        info.mode() & 0o022 == 0 && parent.mode() & 0o022 == 0,
        "Unsafe write permissions"
    );
    Ok(())
}

/// 비공개 임시 파일 생성 후 원자적으로 대상 경로에 교체 저장
/// ACL verifier 호출을 변경하면 MAX_PUBLISH_ACL_CHECKS와 지연 회귀도 갱신합니다.
pub fn write_atomic_private(target: &Path, creds: &McpCredentials) -> Result<()> {
    let parent = target
        .parent()
        .context("Target path has no parent directory")?;
    ensure_safe_parent_directory(parent)?;
    ensure_safe_target_if_exists(target)?;
    set_private_permissions(parent)?;

    let json_bytes = serde_json::to_vec_pretty(creds)?;
    ensure!(
        json_bytes.len() <= MAX_CREDENTIALS_SIZE as usize,
        "Serialized MCP credentials exceed 16KiB limit"
    );

    // 같은 디렉터리에 임시 파일 생성
    let mut temp = tempfile::Builder::new()
        .prefix(".broker-")
        .tempfile_in(parent)
        .context("Cannot create temporary credential file")?;

    // 비밀값을 쓰기 전부터 비공개 권한 적용
    set_private_permissions(temp.path())?;
    temp.as_file_mut().write_all(&json_bytes)?;
    temp.as_file_mut().write_all(b"\n")?;
    temp.as_file().sync_all()?;

    // 임시 파일 권한 사전 검증
    verify_private_permissions(temp.path())?;

    // 대상 경로로 원자적 교체
    let _persisted = temp
        .persist(target)
        .map_err(|_| anyhow::anyhow!("Cannot replace MCP credential file"))?;

    // 교체 후 최종 파일 권한 재적용 및 최종 검증
    set_private_permissions(target)?;
    verify_path_safety(target)?;
    verify_private_permissions(target)?;
    Ok(())
}

/// 기존 자격 파일을 검증하고 로드합니다.
pub fn load(config_path: &Path) -> Result<McpCredentials> {
    let cred_path = path(config_path);
    if !ensure_safe_target_if_exists(&cred_path)? {
        bail!("MCP credentials file does not exist");
    }
    verify_path_safety(&cred_path)?;
    verify_private_permissions(&cred_path)?;

    let file = File::open(&cred_path).context("Cannot open MCP credential file")?;
    let meta = file
        .metadata()
        .context("Cannot inspect opened MCP credential file")?;
    ensure!(meta.is_file(), "Credential path must be a regular file");
    ensure!(
        meta.len() <= MAX_CREDENTIALS_SIZE,
        "Credential file exceeds maximum size limit (16KiB)"
    );

    let mut content = Vec::new();
    file.take(MAX_CREDENTIALS_SIZE + 1)
        .read_to_end(&mut content)
        .context("Cannot read MCP credential file")?;
    ensure!(
        content.len() <= MAX_CREDENTIALS_SIZE as usize,
        "Credential file exceeds maximum size limit (16KiB)"
    );

    let creds: McpCredentials = serde_json::from_slice(&content)
        .map_err(|_| anyhow::anyhow!("Invalid MCP credential file JSON schema"))?;
    validate(&creds)?;
    Ok(creds)
}

/// 브로커 실행 시 MCP 자격 스냅샷을 발행하거나 기존 토큰을 재사용합니다.
///
/// - 유효한 기존 자격이 있으면 `local_token`을 유지하고 `broker_url`만 현재 바인딩된 포트로 갱신합니다.
/// - 자격 파일이 없으면 새로 생성합니다.
/// - 기존 파일이 손상되었거나 권한이 불안전한 경우 조용히 덮어쓰지 않고 명시적 복구 안내와 함께 오류를 반환합니다.
pub fn publish(config_path: &Path, broker_url: &str) -> Result<McpCredentials> {
    let cred_path = path(config_path);
    let parent = cred_path
        .parent()
        .context("Credential path has no parent")?;
    ensure_safe_parent_directory(parent)?;
    let target_exists = ensure_safe_target_if_exists(&cred_path).map_err(|_| {
        anyhow::anyhow!(
            "Existing MCP credential file at '{}' is invalid or has insecure permissions; \
             use 'devlink mcp rotate' or remove the file manually to recover",
            cred_path.display()
        )
    })?;
    set_private_permissions(parent)?;

    let local_token = if target_exists {
        match load(config_path) {
            Ok(existing) => {
                if existing.broker_url == broker_url {
                    return Ok(existing);
                }
                existing.local_token
            }
            Err(_) => {
                bail!(
                    "Existing MCP credential file at '{}' is invalid or has insecure permissions; \
                     use 'devlink mcp rotate' or remove the file manually to recover",
                    cred_path.display()
                );
            }
        }
    } else {
        generate_token()
    };

    let creds = McpCredentials {
        schema_version: SCHEMA_VERSION,
        broker_url: broker_url.to_string(),
        local_token,
    };
    validate(&creds)?;
    write_atomic_private(&cred_path, &creds)?;
    Ok(creds)
}

/// 브로커가 중지된 상태에서만 수동으로 local_token을 회전(재발급)합니다.
///
/// 브로커가 실행 중인 경우에는 상태 일관성을 위해 회전을 거부합니다.
pub fn rotate(config_path: &Path) -> Result<McpCredentials> {
    let state_dir = crate::config::state_directory(config_path);
    ensure_safe_parent_directory(&state_dir)?;
    let cred_path = path(config_path);
    let target_exists = ensure_safe_target_if_exists(&cred_path)?;
    set_private_permissions(&state_dir)?;

    // start/stop과 충돌을 방지하기 위해 control.lock 획득
    let _control_guard = crate::config::lock(&state_dir.join("control.lock"))?;

    // 브로커 실행 여부 검사 (실행 중이면 회전 거부)
    if crate::local::running(config_path)? {
        bail!("Cannot rotate token while broker is running; stop the broker first");
    }

    // 이중 확인을 위한 broker.lock 획득 시도
    let _broker_guard = crate::config::lock(&state_dir.join("broker.lock"))?;

    let broker_url = if target_exists {
        if let Ok(existing) = load(config_path) {
            existing.broker_url
        } else {
            default_broker_url(config_path)?
        }
    } else {
        default_broker_url(config_path)?
    };

    let new_creds = McpCredentials {
        schema_version: SCHEMA_VERSION,
        broker_url,
        local_token: generate_token(),
    };
    validate(&new_creds)?;
    write_atomic_private(&cred_path, &new_creds)?;
    Ok(new_creds)
}

fn default_broker_url(config_path: &Path) -> Result<String> {
    let config = crate::config::Config::load(config_path).unwrap_or_default();
    let port = if config.local_port == 0 || config.local_port == 80 {
        43127
    } else {
        config.local_port
    };
    Ok(format!("http://127.0.0.1:{port}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_local_token() {
        let secret = "test-secret-token-do-not-leak";
        let creds = McpCredentials {
            schema_version: 1,
            broker_url: "http://127.0.0.1:43127".to_string(),
            local_token: secret.to_string(),
        };

        let compact = format!("{:?}", creds);
        let pretty = format!("{:#?}", creds);

        assert!(
            !compact.contains(secret),
            "compact debug output must not contain secret token"
        );
        assert!(
            !pretty.contains(secret),
            "pretty debug output must not contain secret token"
        );
        assert!(
            compact.contains("[REDACTED]"),
            "compact debug output must contain [REDACTED]"
        );
        assert!(
            pretty.contains("[REDACTED]"),
            "pretty debug output must contain [REDACTED]"
        );
        assert!(
            compact.contains("schema_version"),
            "compact debug output must contain schema_version"
        );
        assert!(
            compact.contains("broker_url"),
            "compact debug output must contain broker_url"
        );
        assert!(
            pretty.contains("schema_version"),
            "pretty debug output must contain schema_version"
        );
        assert!(
            pretty.contains("broker_url"),
            "pretty debug output must contain broker_url"
        );
    }

    #[test]
    fn test_publish_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let broker_url = "http://127.0.0.1:43127";
        let creds = publish(&config_path, broker_url).unwrap();
        assert_eq!(creds.schema_version, 1);
        assert_eq!(creds.broker_url, broker_url);
        let loaded = load(&config_path).unwrap();
        assert_eq!(creds, loaded);
    }

    #[test]
    fn credential_read_enforces_size_limit() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let broker_url = "http://127.0.0.1:43127";
        let creds = publish(&config_path, broker_url).expect("initial publish must succeed");
        let cred_path = path(&config_path);

        let initial_len = fs::metadata(&cred_path)
            .expect("must read initial credential metadata")
            .len() as usize;
        assert!(
            initial_len < MAX_CREDENTIALS_SIZE as usize,
            "initial credential file must be smaller than 16,384 bytes"
        );

        let padding_len = (MAX_CREDENTIALS_SIZE as usize) - initial_len;
        {
            let mut f = fs::OpenOptions::new()
                .append(true)
                .open(&cred_path)
                .expect("must open credential file for appending");
            f.write_all(&vec![b' '; padding_len])
                .expect("must pad credential file to 16,384 bytes");
            f.sync_all().expect("must sync 16,384-byte credential file");
        }

        let exact_len = fs::metadata(&cred_path)
            .expect("must read 16,384-byte credential metadata")
            .len();
        assert_eq!(
            exact_len, MAX_CREDENTIALS_SIZE,
            "credential file size must be exactly 16,384 bytes"
        );

        let loaded = load(&config_path).expect("load must succeed at exactly 16,384 bytes");
        assert!(
            loaded == creds,
            "loaded credentials at 16,384 bytes must match published credentials"
        );

        {
            let mut f = fs::OpenOptions::new()
                .append(true)
                .open(&cred_path)
                .expect("must open credential file for +1 byte append");
            f.write_all(b" ")
                .expect("must append one byte to reach 16,385 bytes");
            f.sync_all().expect("must sync 16,385-byte credential file");
        }

        let before_bytes =
            fs::read(&cred_path).expect("must read 16,385-byte credential file before load");
        assert_eq!(
            before_bytes.len(),
            (MAX_CREDENTIALS_SIZE as usize) + 1,
            "credential file size must be 16,385 bytes"
        );

        let over_limit_result = load(&config_path);
        assert!(
            over_limit_result.is_err(),
            "load must reject 16,385-byte credential file"
        );

        let after_bytes =
            fs::read(&cred_path).expect("must read 16,385-byte credential file after load");
        assert!(
            before_bytes == after_bytes,
            "credential file content must remain unchanged after rejected load"
        );
    }

    /// The native verifier must return the same code as the former PowerShell policy
    /// for every owner, ACE shape and verifier mode, including .NET ACL canonicalization.
    #[cfg(windows)]
    #[test]
    fn native_acl_policy_matches_powershell_reference() {
        use std::os::windows::process::CommandExt;
        let sid = current_user_sid().unwrap();
        let owners = [
            sid.as_str(),
            ADMINISTRATORS_SID,
            SYSTEM_SID,
            "S-1-1-0",
            TRUSTED_INSTALLER_SID,
        ];
        let extras = [
            String::new(),
            "(A;;FA;;;SY)(A;ID;FA;;;BA)".into(),
            "(A;;FR;;;WD)".into(),
            "(A;;0x100000;;;WD)".into(),
            "(A;;0x2;;;WD)".into(),
            "(A;;0x40000;;;BU)".into(),
            "(A;OICI;FA;;;WD)".into(),
            "(A;OICIIO;FA;;;WD)".into(),
            "(A;OICIIO;FR;;;WD)".into(),
            "(A;IO;FA;;;WD)".into(),
            "(A;OIIONP;0x2;;;WD)".into(),
            "(A;OICIIO;FA;;;CO)".into(),
            "(A;CIIO;0x2;;;CG)".into(),
            "(A;IO;FA;;;CO)".into(),
            "(A;OICI;FA;;;CO)".into(),
            "(A;OICIIO;GA;;;WD)".into(),
            "(A;;GW;;;WD)".into(),
            "(A;;0x0;;;WD)".into(),
            "(A;ID;0x1301bf;;;BU)".into(),
            "(D;;FA;;;WD)".into(),
            format!("(A;;FA;;;{TRUSTED_INSTALLER_SID})"),
        ];
        let user = format!("(A;;FA;;;{sid})");
        let mut dacls: Vec<String> = extras.iter().map(|e| format!("D:P{user}{e}")).collect();
        dacls.extend(["D:NO_ACCESS_CONTROL".into(), "D:P".into()]);
        let mut cases = Vec::new();
        for owner in owners {
            for dacl in &dacls {
                for bits in 0..32u8 {
                    let flag = |bit: u8| bits & (1 << bit) != 0;
                    let mode = AclMode {
                        is_file: flag(0),
                        others_may_read: flag(1),
                        source: flag(2),
                        check_owner: flag(3),
                        config_file: flag(4),
                    };
                    cases.push((format!("O:{owner}G:SY{dacl}"), mode));
                }
            }
        }
        let input: String = cases
            .iter()
            .map(|(sddl, m)| {
                let b = |v: bool| if v { '1' } else { '0' };
                format!(
                    "{sddl}\t{}\t{}\t{}\t{}\t{}\n",
                    b(m.is_file),
                    b(m.others_may_read),
                    b(m.source),
                    b(m.check_owner),
                    b(m.config_file)
                )
            })
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let cases_path = dir.path().join("cases.tsv");
        fs::write(&cases_path, input).unwrap();
        const HARNESS: &str = r#"
$ErrorActionPreference = 'Stop'
$sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$out = [System.Collections.Generic.List[string]]::new()
$out.Add($sid)
foreach ($line in [System.IO.File]::ReadAllLines($env:DEVLINK_ACL_CASES)) {
  $f = $line.Split("`t")
  $isFile = $f[1] -eq '1'
  $acl = if ($isFile) { [System.Security.AccessControl.FileSecurity]::new() } else { [System.Security.AccessControl.DirectorySecurity]::new() }
  $acl.SetSecurityDescriptorSddlForm($f[0])
  $out.Add([string](Get-DevLinkAclFailure $acl $sid $isFile ($f[2] -eq '1') ($f[3] -eq '1') ($f[4] -eq '1') ($f[5] -eq '1')))
}
[Console]::Out.Write(($out -join "`n"))
"#;
        let output = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("{WINDOWS_ACL_POLICY}\n{HARNESS}"),
            ])
            .env("DEVLINK_ACL_CASES", &cases_path)
            .stdin(std::process::Stdio::null())
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let mut lines = stdout.lines();
        assert_eq!(
            lines.next(),
            Some(sid.as_str()),
            "token SID differs from .NET"
        );
        let expected: Vec<&str> = lines.collect();
        assert_eq!(expected.len(), cases.len());
        let mut seen = std::collections::BTreeSet::new();
        for ((sddl, mode), expected) in cases.iter().zip(expected) {
            let descriptor = crate::win_acl::parse_sddl(sddl).unwrap();
            let actual = acl_failure(&descriptor, &sid, *mode).to_string();
            assert_eq!(actual, expected, "{sddl} {mode:?}");
            seen.insert(actual);
        }
        // Every verdict must be exercised so the comparison cannot pass vacuously.
        assert_eq!(seen.into_iter().collect::<Vec<_>>(), ["0", "2", "3"]);
    }

    #[cfg(windows)]
    fn assert_private_windows_dacl(path: &Path, sid: &str) {
        use std::os::windows::process::CommandExt;
        // Inspect the descriptor's semantics independently of the native writer.
        // The SDDL auto-inheritance marker and SID formatting can vary by runner.
        const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$p = $env:DEVLINK_TEST_ACL_PATH
$isDir = [System.IO.Directory]::Exists($p)
$acl = if ($isDir) { [System.IO.Directory]::GetAccessControl($p) } else { [System.IO.File]::GetAccessControl($p) }
$raw = [System.Security.AccessControl.RawSecurityDescriptor]::new($acl.GetSecurityDescriptorBinaryForm(), 0)
$dacl = $raw.DiscretionaryAcl
$flags = 0
if ($isDir) {
  $flags = [int][System.Security.AccessControl.AceFlags]::ContainerInherit -bor [int][System.Security.AccessControl.AceFlags]::ObjectInherit
}
if (-not $acl.AreAccessRulesProtected -or $null -eq $dacl -or $dacl.Count -ne 1) {
  throw "Expected a protected DACL with one ACE; got $($acl.GetSecurityDescriptorSddlForm('Access'))"
}
$ace = $dacl[0]
if ($ace.AceType -ne [System.Security.AccessControl.AceType]::AccessAllowed -or
    [int]$ace.AceFlags -ne $flags -or
    $ace.AccessMask -ne [int][System.Security.AccessControl.FileSystemRights]::FullControl -or
    $ace.SecurityIdentifier.Value -ne $env:DEVLINK_TEST_ACL_SID) {
  throw "Unexpected private ACE; got $($acl.GetSecurityDescriptorSddlForm('Access'))"
}
"#;
        let output = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
            .env("DEVLINK_TEST_ACL_PATH", path)
            .env("DEVLINK_TEST_ACL_SID", sid)
            .stdin(std::process::Stdio::null())
            .creation_flags(0x08000000)
            .output()
            .expect("Failed to inspect the private Windows DACL");
        assert!(
            output.status.success(),
            "Private Windows DACL mismatch: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(windows)]
    #[test]
    fn native_private_permissions_leave_only_a_protected_user_ace() {
        let sid = current_user_sid().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("broker.json");
        fs::write(&file, b"{}").unwrap();
        set_private_permissions(dir.path()).unwrap();
        set_private_permissions(&file).unwrap();
        assert_private_windows_dacl(dir.path(), &sid);
        assert_private_windows_dacl(&file, &sid);
        verify_private_permissions(&file).unwrap();
        verify_directory_permissions(dir.path()).unwrap();
        // A later explicit grant is replaced, not kept beside the user ACE.
        let grant = std::process::Command::new("icacls.exe")
            .arg(&file)
            .args(["/grant", "*S-1-1-0:(R)"])
            .output()
            .unwrap();
        assert!(grant.status.success());
        assert!(verify_private_permissions(&file).is_err());
        set_private_permissions(&file).unwrap();
        assert_private_windows_dacl(&file, &sid);
    }

    #[cfg(windows)]
    #[test]
    fn windows_config_owner_policy_keeps_private_files_and_writer_checks_strict() {
        use std::os::windows::process::CommandExt;
        const CASES: &str = r#"
$ErrorActionPreference = 'Stop'
$sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$admin = 'S-1-5-32-544'
$system = 'S-1-5-18'
$everyone = 'S-1-1-0'
function New-TestAcl([string]$owner) {
  $acl = [System.Security.AccessControl.FileSecurity]::new()
  $acl.SetOwner([System.Security.Principal.SecurityIdentifier]::new($owner))
  $acl.AddAccessRule([System.Security.AccessControl.FileSystemAccessRule]::new(
    [System.Security.Principal.SecurityIdentifier]::new($sid), 'FullControl', 'Allow'))
  return $acl
}
foreach ($owner in @($sid, $admin, $system, $everyone)) {
  $acl = New-TestAcl $owner
  $configExpected = if ($owner -in @($sid, $admin)) { 0 } else { 2 }
  $privateExpected = if ($owner -eq $sid) { 0 } else { 2 }
  if ((Get-DevLinkAclFailure $acl $sid $true $true $false $true $true) -ne $configExpected) { throw 'config owner policy' }
  if ((Get-DevLinkAclFailure $acl $sid $true $false) -ne $privateExpected) { throw 'private owner policy' }
}
# Public Host files and project directories keep the stricter user owner policy;
# only config enables the explicit Administrators exception. Source/state paths
# retain their separate system-owned mode from the MCP configuration provider.
$acl = New-TestAcl $admin
if ((Get-DevLinkAclFailure $acl $sid $true $true $false $true $false) -ne 2) { throw 'host owner policy' }
if ((Get-DevLinkAclFailure $acl $sid $false $true $false $true $false) -ne 2) { throw 'project owner policy' }
$acl = New-TestAcl $system
if ((Get-DevLinkAclFailure $acl $sid $true $true $true $true $false) -ne 0) { throw 'source owner policy' }
if ((Get-DevLinkAclFailure $acl $sid $false $true $true $true $false) -ne 0) { throw 'state owner policy' }
$acl = New-TestAcl $admin
$acl.AddAccessRule([System.Security.AccessControl.FileSystemAccessRule]::new(
  [System.Security.Principal.SecurityIdentifier]::new($everyone), 'Read', 'Allow'))
if ((Get-DevLinkAclFailure $acl $sid $true $true $false $true $true) -ne 0) { throw 'public config read' }
$acl.AddAccessRule([System.Security.AccessControl.FileSystemAccessRule]::new(
  [System.Security.Principal.SecurityIdentifier]::new($everyone), 'Modify', 'Allow'))
if ((Get-DevLinkAclFailure $acl $sid $true $true $false $true $true) -ne 3) { throw 'config writer policy' }
foreach ($othersMayRead in @($false, $true)) {
  if ((Get-DevLinkAclFailure $acl $sid $false $othersMayRead) -ne 3) { throw 'parent writer policy' }
}
"#;
        let script = format!("{WINDOWS_ACL_POLICY}\n{CASES}");
        let output = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .stdin(std::process::Stdio::null())
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Exercise the Rust mode mapping as well as the shared PowerShell policy.
        // A source-only trusted writer must never become trusted for config.
        let dir = tempfile::tempdir().unwrap();
        set_private_permissions(dir.path()).unwrap();
        let config = dir.path().join("config.json");
        fs::write(&config, b"{}").unwrap();
        set_private_permissions(&config).unwrap();
        let grant = std::process::Command::new("icacls.exe")
            .arg(&config)
            .arg("/grant")
            .arg("*S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464:(M)")
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(
            grant.status.success(),
            "Cannot prepare the isolated ACL fixture"
        );
        assert!(verify_trusted_writer_permissions(&config).is_err());
        assert!(verify_private_permissions(&config).is_err());
        assert!(verify_public_file_permissions(&config, true).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn test_windows_verify_private_permissions_three_cases() {
        // Case 1: 정상 비공개 파일·부모 → 허용
        let dir1 = tempfile::tempdir().unwrap();
        let file1 = dir1.path().join("broker.json");
        std::fs::write(&file1, b"test").unwrap();
        set_private_permissions(dir1.path()).unwrap();
        set_private_permissions(&file1).unwrap();
        let res1 = verify_private_permissions(&file1);
        assert!(res1.is_ok(), "Case 1 failed: {:?}", res1.err());

        // Case 2: 파일에 Everyone 읽기 Allow 추가 → 거부
        let status2 = std::process::Command::new("icacls.exe")
            .arg(&file1)
            .arg("/grant")
            .arg("*S-1-1-0:(R)")
            .status()
            .unwrap();
        assert!(status2.success());
        let res2 = verify_private_permissions(&file1);
        assert!(res2.is_err(), "Case 2 expected error, got Ok");

        // Case 3: 부모에 Everyone 쓰기 Allow 추가 → 거부
        let dir3 = tempfile::tempdir().unwrap();
        let file3 = dir3.path().join("broker.json");
        std::fs::write(&file3, b"test").unwrap();
        set_private_permissions(dir3.path()).unwrap();
        set_private_permissions(&file3).unwrap();
        assert!(verify_private_permissions(&file3).is_ok());

        let status3 = std::process::Command::new("icacls.exe")
            .arg(dir3.path())
            .arg("/grant")
            .arg("*S-1-1-0:(W)")
            .status()
            .unwrap();
        assert!(status3.success());
        let res3 = verify_private_permissions(&file3);
        assert!(res3.is_err(), "Case 3 expected error, got Ok");
    }

    #[test]
    fn broker_url_validation_matches_mcp_reader() {
        assert!(is_valid_broker_url("http://127.0.0.1:43127"));
        assert!(is_valid_broker_url("http://127.0.0.1:1"));
        assert!(is_valid_broker_url("http://127.0.0.1:79"));
        assert!(is_valid_broker_url("http://127.0.0.1:81"));
        assert!(is_valid_broker_url("http://127.0.0.1:65535"));

        // Port 80 is normalized to empty url.port by WHATWG URL in MCP reader and rejected
        assert!(!is_valid_broker_url("http://127.0.0.1:80"));
        assert!(!is_valid_broker_url("http://127.0.0.1:+81"));
        assert!(!is_valid_broker_url("http://127.0.0.1:+43127"));
        assert!(!is_valid_broker_url("http://127.0.0.1:0"));
        assert!(!is_valid_broker_url("http://127.0.0.1:080"));
        assert!(!is_valid_broker_url("http://127.0.0.1:043127"));
        assert!(!is_valid_broker_url("http://127.0.0.1:65536"));
        assert!(!is_valid_broker_url("http://127.0.0.1:43127/"));
        assert!(!is_valid_broker_url("http://localhost:43127"));
    }

    #[cfg(windows)]
    fn get_sddl(path: &Path) -> String {
        use std::os::windows::process::CommandExt;
        const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$p = $env:DEVLINK_SDDL_PATH
$acl = if ([System.IO.Directory]::Exists($p)) { [System.IO.Directory]::GetAccessControl($p) } else { [System.IO.File]::GetAccessControl($p) }
[Console]::Write($acl.GetSecurityDescriptorSddlForm('All'))
"#;
        let output = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
            .env("DEVLINK_SDDL_PATH", path)
            .stdin(std::process::Stdio::null())
            .creation_flags(0x08000000)
            .output()
            .expect("Failed to get SDDL");
        assert!(output.status.success(), "powershell SDDL query failed");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[cfg(windows)]
    #[test]
    fn test_publish_rejects_junction_parent_and_preserves_target() {
        use std::os::windows::process::CommandExt;

        let temp_root = tempfile::tempdir().unwrap();
        let target_dir = temp_root.path().join("target_dir");
        std::fs::create_dir_all(&target_dir).unwrap();

        let sentinel_file = target_dir.join("sentinel.txt");
        std::fs::write(&sentinel_file, b"original-content").unwrap();

        // target_dir 및 기존 파일의 초기 권한(SDDL) 기록
        let target_dir_sddl_before = get_sddl(&target_dir);
        let sentinel_sddl_before = get_sddl(&sentinel_file);

        let config_dir = temp_root.path().join("config_dir");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.json");
        std::fs::write(&config_path, b"{}").unwrap();

        // config_dir/.devlink-runtime을 target_dir을 가리키는 junction으로 생성
        let junction_path = crate::config::state_directory(&config_path);
        let status = std::process::Command::new("cmd")
            .args([
                "/c",
                "mklink",
                "/J",
                junction_path.to_str().unwrap(),
                target_dir.to_str().unwrap(),
            ])
            .stdin(std::process::Stdio::null())
            .creation_flags(0x08000000)
            .status()
            .expect("mklink /J execution failed");
        assert!(status.success(), "Failed to create directory junction");

        // 정리 시 링크 대상을 재귀 삭제하지 않도록 junction을 먼저 분리/삭제하는 RAII 가드
        struct JunctionCleanup<'a>(&'a Path);
        impl Drop for JunctionCleanup<'_> {
            fn drop(&mut self) {
                use std::os::windows::process::CommandExt;
                let _ = std::process::Command::new("cmd")
                    .args(["/c", "rmdir", self.0.to_str().unwrap()])
                    .stdin(std::process::Stdio::null())
                    .creation_flags(0x08000000)
                    .status();
            }
        }
        let _cleanup = JunctionCleanup(&junction_path);

        // publish 및 rotate 호출: junction 부모 경로로 인해 거부되어야 함
        let res = publish(&config_path, "http://127.0.0.1:43127");
        assert!(
            res.is_err(),
            "publish must return Err when parent directory is a junction"
        );
        let rotate_res = rotate(&config_path);
        assert!(
            rotate_res.is_err(),
            "rotate must return Err when parent directory is a junction"
        );

        // 링크 대상에 자격 파일이 생성되지 않았는지 확인
        let leaked_file = target_dir.join(CREDENTIALS_FILE_NAME);
        assert!(
            !leaked_file.exists(),
            "credentials file must not be created in junction target directory"
        );

        // 링크 대상의 기존 파일 내용 보존 확인
        let sentinel_content = std::fs::read(&sentinel_file).unwrap();
        assert_eq!(
            sentinel_content, b"original-content",
            "existing file in target must be preserved"
        );

        // 링크 대상 디렉터리 및 기존 파일의 권한(SDDL)이 변경되지 않았는지 확인
        let target_dir_sddl_after = get_sddl(&target_dir);
        let sentinel_sddl_after = get_sddl(&sentinel_file);
        assert_eq!(
            target_dir_sddl_before, target_dir_sddl_after,
            "target directory permissions must remain untouched"
        );
        assert_eq!(
            sentinel_sddl_before, sentinel_sddl_after,
            "existing file permissions must remain untouched"
        );
    }
}
