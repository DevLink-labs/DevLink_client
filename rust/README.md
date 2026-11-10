# DevLink

Rust CLI와 독립 로컬 브로커입니다. CLI는 설정과 프로세스 수명 주기를 관리하고, 브로커는 로컬 제어 API와 중앙 서버 WebSocket 연결을 관리합니다.

실제 중앙 사용자 로그인·팀 선택 이후 Agent 등록/기존 ID 재사용·토큰 발급·브로커 기동·MCP 자격 파일 연결은 [Agent bootstrap](docs/agent-bootstrap.md)을 참고합니다.

`devlink setup --help`에서 [간편설정 실행 흐름과 Agent 등록·복구](docs/setup.md)를 확인할 수 있습니다. 설정·세션·팀을 확인하고 프로젝트·설치 도구를 준비한 뒤 기존 Agent를 재사용합니다. 새 생성은 `--create-agent`, 기존 본인 ID 선택·pending 복구는 `--agent-id <ID>`로 명시합니다. 새 Agent는 최초 토큰 발급 1회까지 연결합니다. 기존 Agent는 정상 토큰을 검증해 재사용하고, 누락/인증 거부 토큰은 `--issue-token`으로 발급을 허용합니다. 응답 유실은 ID를 보존하고 자동 재발급하지 않습니다. 프로젝트 MCP 설정은 계획 확인 후 적용하며, 비대화형에서는 `--apply-mcp`가 필요합니다. `--mcp-mode manual`은 설정 예시만 반환합니다. 최종 연결 진단은 후속 작업입니다.

이 디렉터리는 `devlink-cli`(실행 파일)와 `devlink-broker`(라이브러리)로 구성한 Cargo workspace입니다. CLI가 브로커 라이브러리를 참조하며 브로커는 CLI에 의존하지 않습니다. 아래 명령은 저장소 루트에서 `cd rust`로 이동한 뒤 실행합니다.

2026-10-29: 중앙 `TASK_CREATED` 수신 → 디스크 수신함 저장 → `ACK` 전송을 추가했습니다. 재접속·재시작 후 중복 수신을 구분하고, 저장 실패는 ACK 없이 `InboxFailed`로 중단합니다. 상태·완료·목록·결과의 Rust HTTP provider도 구현되어 있으며, MCP 최종 DTO와 실제 Host 전체 왕복은 후속 범위입니다. [수신함·ACK와 Mock 데모](docs/task-inbox.md)에 보관 위치·제한·실패 정책과 공동 계약이 필요한 항목을 정리했습니다.

## 릴리스로 설치

소스를 빌드하지 않고 GitHub Release의 압축 파일로 설치할 수 있습니다. 압축 파일 이름은 `devlink-<버전>-<target>`이고 `SHA256SUMS`가 함께 올라갑니다.

| OS | 파일 |
| --- | --- |
| Windows x64 | `devlink-<버전>-x86_64-pc-windows-msvc.zip` |
| Linux x64 (glibc) | `devlink-<버전>-x86_64-unknown-linux-gnu.tar.gz` |
| macOS Apple Silicon | `devlink-<버전>-aarch64-apple-darwin.tar.gz` |

Linux 바이너리는 GitHub `ubuntu-latest` runner에서 빌드합니다. 이 레이블의 OS 버전은 바뀔 수 있으므로 해당 릴리스의 빌드 로그에서 실제 이미지 버전을 확인합니다. 빌드 환경보다 glibc가 오래된 배포판(Ubuntu 20.04, Debian 11, RHEL 8 등)에서는 `GLIBC_… not found`로 실행되지 않을 수 있습니다. 이때는 소스에서 빌드합니다. macOS Intel 번들은 아직 없습니다.

압축을 풀면 `devlink` 실행 파일 옆에 MCP 서버 런타임(`mcp-server/` 아래 `package.json`, `dist/`, `node_modules/`)이 있습니다. `devlink setup`은 실행 파일 옆의 `mcp-server/dist/index.js`를 MCP 진입점으로 찾으므로 두 항목을 같은 폴더에 두고 함께 옮겨야 합니다. 실행 파일만 다른 폴더로 복사하거나 심볼릭 링크로 PATH에 넣지 말고, 압축을 푼 폴더 자체를 PATH에 추가합니다(macOS는 링크 경로를 실행 파일 위치로 볼 수 있습니다). Node.js는 포함하지 않습니다.

1. **압축 해제.** 현재 사용자만 쓸 수 있는 홈 아래 폴더에 풉니다. setup은 MCP 진입점과 상위 폴더를 다른 사용자가 바꿀 수 없어야 진행합니다([Windows paths](docs/setup.md#windows-paths)).

   ```powershell
   # Windows (PowerShell). 내려받은 폴더에서 실행합니다.
   (Get-FileHash .\devlink-<버전>-x86_64-pc-windows-msvc.zip -Algorithm SHA256).Hash  # SHA256SUMS와 비교
   Expand-Archive .\devlink-<버전>-x86_64-pc-windows-msvc.zip -DestinationPath $HOME
   Rename-Item "$HOME\devlink-<버전>-x86_64-pc-windows-msvc" "$HOME\DevLink"
   ```

   ```bash
   # Linux / macOS
   sha256sum -c SHA256SUMS --ignore-missing    # macOS: shasum -a 256 -c SHA256SUMS --ignore-missing
   tar -xzf devlink-<버전>-<target>.tar.gz -C "$HOME"
   mv "$HOME/devlink-<버전>-<target>" "$HOME/devlink"
   ```

   macOS에서 브라우저로 내려받았다면 Gatekeeper가 서명되지 않은 실행 파일을 막을 수 있습니다. 내용을 확인했다면 `xattr -dr com.apple.quarantine "$HOME/devlink"`로 격리 속성을 지웁니다.

2. **PATH 추가.** 새 터미널에서 `devlink --version`이 버전을 출력하면 됩니다.

   ```powershell
   # Windows: 사용자 PATH에 한 번만 추가한 뒤 새 PowerShell을 엽니다.
   # 사용자 PATH에 %VAR% 형태 항목이 있으면 이 방법 대신 "시스템 환경 변수 편집" 창에서 추가합니다.
   $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
   if (($userPath -split ';') -notcontains "$HOME\DevLink") {
     [Environment]::SetEnvironmentVariable('Path', "$userPath;$HOME\DevLink", 'User')
   }
   ```

   ```bash
   # Linux / macOS: 셸 시작 파일(~/.bashrc, ~/.zshrc 등)에 추가합니다.
   export PATH="$HOME/devlink:$PATH"
   ```

3. **Node 24 확인.** MCP 서버는 Node.js 24 이상으로 실행됩니다. `node --version`이 `v24` 이상이어야 합니다. 없으면 [Node.js](https://nodejs.org/) 24 LTS를 설치합니다. setup은 `--node-path`로 지정한 Node, 선택한 Host가 Node 스크립트(npm 설치)면 그 Host를 실행하는 Node, PATH의 Node 순서로 찾습니다. 따라서 Host를 실행하는 Node가 24 미만이면 PATH에 24가 있어도 실패할 수 있습니다. 이때는 `--node-path <Node 24 절대 경로>`를 지정합니다.

4. **`devlink setup` 실행.** 연결할 프로젝트 폴더에서 실행합니다. 로그인·팀·도구·Agent 선택과 프로젝트 MCP 설정은 [setup 문서](docs/setup.md)를 따릅니다.

   ```powershell
   cd <프로젝트 폴더>
   devlink setup
   ```

**업데이트.** Codex·Claude Code를 닫고 `devlink stop`으로 브로커를 멈춥니다. 이전 파일이 남지 않도록 기존 폴더를 지우고 새 압축 파일을 같은 이름으로 풉니다. 그다음 프로젝트 폴더에서 `devlink setup`을 다시 실행해 런타임을 확인합니다. 프로젝트 MCP 설정에는 Node와 MCP 진입점의 절대 경로가 들어가므로, 폴더 위치는 그대로 둡니다.

```powershell
# Windows
devlink stop
Remove-Item -Recurse -Force "$HOME\DevLink"
Expand-Archive .\devlink-<새 버전>-x86_64-pc-windows-msvc.zip -DestinationPath $HOME
Rename-Item "$HOME\devlink-<새 버전>-x86_64-pc-windows-msvc" "$HOME\DevLink"
```

```bash
# Linux / macOS
devlink stop
rm -rf "$HOME/devlink"
tar -xzf devlink-<새 버전>-<target>.tar.gz -C "$HOME"
mv "$HOME/devlink-<새 버전>-<target>" "$HOME/devlink"
```

릴리스는 `.github/workflows/release.yml`이 `v*` 태그 push 때 만듭니다. 태그는 `rust/Cargo.toml`의 workspace 버전, `packages/mcp-server/package.json`의 버전과 같아야 합니다. 릴리스 워크플로는 번들 실행 확인만 하고 전체 테스트는 돌리지 않으므로, 태그는 master CI를 통과한 커밋에 답니다. 같은 태그의 Release가 이미 있으면 `gh release create`가 실패하므로 지운 뒤 다시 실행합니다. 로컬에서 같은 번들을 만들려면 저장소 루트에서 `node .github/scripts/package-release.mjs`를 실행합니다. 결과는 `dist/release/`에 생깁니다.

## Windows 개발 환경

1. Visual Studio 2022 Build Tools의 **Desktop development with C++** 워크로드와 Windows SDK를 설치합니다.
2. [공식 Rust 설치 프로그램](https://www.rust-lang.org/tools/install)으로 MSVC stable 도구 체인을 설치합니다.
3. 새 PowerShell에서 아래 명령을 실행합니다. PATH가 갱신되지 않았다면 `$env:USERPROFILE\.cargo\bin\cargo.exe`를 사용합니다.

```powershell
cargo build --locked --workspace
cargo test --locked --workspace --all-targets
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo build --release --locked --workspace
.\target\release\devlink.exe --help
```

테스트 실행 방식(2026-11-10 측정):

- Windows에서는 위처럼 스레드 옵션 없이 돌려도 된다. 로컬 22코어에서 전체가 약 6분이고, 예전의 `-- --test-threads=1`(직렬)은 약 11분이다. 병렬 실행 3회에서 실패는 없었다.
- Linux·macOS에서 `cargo test`를 돌릴 때는 `-- --test-threads=1`을 붙인다. GitHub 러너에서 스레드 병렬로 돌리면 매번 다른 테스트가 자기 임시 디렉터리의 잠금을 "사용 중"으로 받아 실패했다(Ubuntu 2회 모두, macOS 3회 중 1회). 원인은 추정만 했다(`.github/workflows/ci.yml`의 주석).
- CI는 `cargo-nextest`로 실행한다(`cargo nextest run --workspace --all-targets --locked --no-fail-fast`). 테스트마다 별도 프로세스라 위 실패가 나오지 않았고, 3개 OS에서 직렬보다 빠르다. 로컬에 설치돼 있으면 같은 명령을 쓸 수 있다.

`rust-toolchain.toml`은 stable, rustfmt, clippy를 선택합니다. 의존성 버전은 이 디렉터리의 `Cargo.lock`으로 고정하며 공통 의존성은 workspace의 `Cargo.toml`에서 관리합니다. 빌드 산출물은 `rust/target/`에 생성됩니다. 최근 Windows MSVC 검증과 고정 중앙·DB 결과는 [검증 요약](docs/verification.md)에 실행별로 보관합니다. -95 후속 보완의 전체 Rust 회귀는 168건 통과·2건 제외였고, 문서 정리 과정에서 다시 실행한 결과는 아닙니다. Ubuntu·CI·운영 Host 전체 왕복은 별도 검증입니다.

Windows x64 MSVC 빌드는 `.cargo/config.toml`의 `+crt-static` 설정으로 C 런타임을 정적으로 연결합니다. 배포받는 PC에 Visual C++ 재배포 패키지를 요구하지 않도록 한 설정입니다. `rust` 디렉터리에서 빌드해야 이 설정을 읽으며, `RUSTFLAGS`로 재정의할 때도 정적 연결 설정을 유지해야 합니다. 데스크톱 패키징은 완성된 CLI의 일반·지연 DLL 가져오기 표를 검사해 Visual C++ 런타임 DLL 의존성이 남으면 중단합니다.

통합 테스트는 로컬 TCP 연결을 사용합니다. 네트워크가 제한된 샌드박스에서 Windows 오류 `10013` 또는 브로커 시작 시간 초과가 발생하면 로컬 통신이 허용된 일반 PowerShell이나 승인된 샌드박스 외부 실행으로 검증하세요.

## 빠른 실행

첫 번째 PowerShell에서 Mock 중앙 서버를 실행합니다. 예제 값은 개발용이며 실제 인증 토큰은 설정 파일이나 명령행 인수에 넣지 않습니다.

```powershell
$env:DEVLINK_TOKEN = 'local-development-token'
cargo run -p devlink-broker --example mock_server
```

두 번째 PowerShell에서 **동일한** 토큰을 설정하고 CLI를 실행합니다.

```powershell
$env:DEVLINK_TOKEN = 'local-development-token'
cargo run -p devlink-cli -- init
cargo run -p devlink-cli -- config show
cargo run -p devlink-cli -- start
cargo run -p devlink-cli -- status
cargo run -p devlink-cli -- stop
```

빌드한 실행 파일을 PATH에 추가하면 `devlink init`, `devlink start`, `devlink status`, `devlink stop`으로 사용할 수 있습니다. CLI가 종료되어도 브로커는 계속 실행되며 Windows에서 별도 콘솔 창을 만들지 않습니다. 운영체제 재부팅 후 자동 실행이나 Windows 서비스 등록은 포함하지 않습니다.

Mock 서버를 Ctrl+C로 종료하면 `status`가 `Reconnecting`으로 바뀝니다. 같은 포트·토큰으로 서버를 다시 실행하면 `Connected`로 돌아옵니다. `Connected`는 `/ws/broker` WebSocket Upgrade 인증(`Authorization: Bearer <DEVLINK_TOKEN>`) 후 유효한 `BROKER_READY` 수신(확인된 로컬 Agent 상태가 있는 경우 초기 `AGENT_PRESENCE` 보고 포함; 로컬 상태 미확인 시 추측한 `AGENT_PRESENCE`는 전송하지 않음)까지 마친 상태입니다. Upgrade HTTP `401`/`403`은 `AuthFailed`, 동일 Agent의 새 연결 교체(`4002`)는 `Replaced`, 메시지/정책 위반(`1008` 또는 잘못된 `BROKER_READY`)은 `ProtocolViolation`이 되며 자동 재시도하지 않습니다. 이 경우 토큰이나 설정을 고친 뒤 `stop`/`start`로 다시 시작합니다. Close `4001`은 하트비트 만료이며 `autoReconnect` 설정에 따라 재접속합니다.

중앙 연결·로컬 HTTP·세션 관리자는 같은 `CentralState`를 공유합니다. 검증된 세션의 생성·bind·해제·만료에서 활성 bound 소유자 유무로 presence를 계산하고 `set_presence(Online/Offline)`에 게시합니다. READY 이후 변경 보고, 동일 상태 중복 생략, 재접속 시 최신 상태 재보고를 사용합니다. 기본 상태는 미확인(`None`)이며 health·거부된 요청만으로 상태를 확정하지 않습니다. READY 신원 공급부와 고정 중앙·DB의 검증은 연결했으며, 실제 운영 배포·Host 전체 업무 왕복은 별도입니다.

## 설정

기본 경로는 `$HOME/.devlink/config.json`입니다. 모든 명령에 `--config <FILE>`을 지정할 수 있습니다. 공백·한글을 포함하는 경로도 지원합니다.

내부 파일은 설정 디렉터리 아래의 `.devlink-runtime/`에 따로 저장합니다. 설정 파일명으로 `runtime.json`, `broker.lock`, `control.lock`을 사용해도 내부 파일과 충돌하지 않습니다.

```powershell
cargo run -p devlink-cli -- --config .\.devlink\config.json init
cargo run -p devlink-cli -- --config .\.devlink\config.json config set serverUrl ws://127.0.0.1:8080
cargo run -p devlink-cli -- --config .\.devlink\config.json config set autoReconnect false
cargo run -p devlink-cli -- --config .\.devlink\config.json start
```

`--config`를 사용한 인스턴스는 `status`/`stop`에도 같은 경로를 지정해야 합니다. **설정 디렉터리 하나당 브로커 하나**를 실행합니다. 별도 인스턴스는 별도 디렉터리를 사용하고 `localPort`를 다르게 지정하거나 `0`으로 설정합니다.

| 키 | 기본값 | 제약 |
| --- | --- | --- |
| `serverUrl` | `wss://api.devlink.ai.kr` | ws/wss URL. 경로는 비어 있거나 `/` 또는 `/ws/broker`만 허용. 사용자 정보·쿼리·프래그먼트 금지 |
| `agent` | `codex` | 영문·숫자·`-`·`_`, 1~64자 |
| `autoReconnect` | `true` | JSON boolean |
| `localPort` | `43127` | 0~65535 중 80 제외, 0은 OS 자동 할당 |
| `reconnectInitialMs` | `2000` | 50 이상, 최대 지연 이하 |
| `reconnectMaxMs` | `30000` | 초기 지연 이상, 300000 이하 |
| `authTimeoutMs` | `5000` | 100~60000 |
| `taskMessageSyncIntervalMs` | `60000` | 1000~3600000. 질문·답변 안전망 pull 주기, 실제 간격은 ±1/6 무작위 ([task-messages](docs/task-messages.md)) |
| `taskOverdueMinutes` | `30` | 0~10080. 보낸 작업에 이 시간 동안 최종 상태 알림이 없으면 로컬 `TASK_OVERDUE` 알림. 0이면 끔 ([local-updates](docs/local-updates.md#보낸-작업-지연-알림-rcv-005)) |

`config`와 `config show`는 동일합니다. `config set <키> <값>`은 검증한 뒤 임시 파일을 원자적으로 교체합니다. 잘못된 값·알 수 없는 키는 실패하며 기존 파일을 유지합니다. 설정 변경은 브로커 재시작 후 반영됩니다. `init`은 기존 파일을 덮어쓰지 않습니다. TypeScript의 `serverUrl`/`agent`/`autoReconnect` 3필드 설정도 읽을 수 있지만 기존 `.devlink-practice` 파일을 자동 이동하지는 않습니다.

중앙 서버 인증에는 bootstrap으로 저장한 Agent 토큰 또는 legacy `DEVLINK_TOKEN` 환경 변수를 사용합니다. 우선순위와 재인증은 [Agent bootstrap](docs/agent-bootstrap.md)을 따릅니다. 토큰은 필수이며 설정·상태·서버 응답·오류에 출력하지 않습니다. CLI 제어 토큰은 `.devlink-runtime/runtime.json`, MCP 로컬 토큰은 같은 디렉터리의 `broker.json`에 별도로 저장합니다. `broker.lock`과 `control.lock`도 `.devlink-runtime/`에 저장합니다. 기본 설정에서는 `$HOME/.devlink/.devlink-runtime/`이며 `--config`를 지정하면 해당 설정 디렉터리를 기준으로 합니다. 이 디렉터리는 사용자 전용으로 관리하고 공유하거나 커밋하지 마세요.

### MCP 자격 파일과 토큰

브로커는 기동 시 `schemaVersion: 1`, `brokerUrl`, `localToken`의 완결된 자격 파일을 원자적으로 발행합니다. Agent 프로필이 있으면 `agentId`도 발행하며, 중앙 ID의 저장·재사용은 [Agent bootstrap](docs/agent-bootstrap.md)을 따릅니다. 자격 파일과 직계 부모의 심볼릭 링크, Windows junction·reparse point를 거부하고 Windows 소유권·DACL을 설정·검증합니다. Unix는 디렉터리 0700·파일 0600과 소유권·권한 검사 코드가 있으나 이 Windows 환경에서 Unix 실행은 검증하지 않았습니다.

```powershell
# 경로만 출력합니다. 자격 파일을 만들거나 브로커를 시작하지 않습니다.
cargo run -p devlink-cli -- mcp path
# 브로커 시작 후 발행된 파일의 경로를 MCP Host에 전달합니다.
$env:DEVLINK_BROKER_FILE = (cargo run -p devlink-cli -- mcp path)

# 토큰 회전은 브로커를 중지한 상태에서만 가능합니다.
cargo run -p devlink-cli -- stop
cargo run -p devlink-cli -- mcp rotate
cargo run -p devlink-cli -- start
# 실행 중이던 MCP 프로세스도 다시 시작합니다.
```

사용자 지정 설정을 쓴다면 위 명령에도 같은 `--config <FILE>`을 지정합니다. 정상 종료·강제 종료 후 재시작에는 기존 `localToken`을 유지하고, `mcp rotate`는 새 토큰을 발행합니다. 손상되거나 권한이 안전하지 않은 기존 자격은 기동 시 조용히 덮어쓰지 않고 오류로 처리합니다. CLI의 `controlToken`은 기동마다 바뀌며 `GET /health`·`POST /shutdown`에 사용합니다. MCP의 `localToken`은 현재 `GET /health`만 허용하고 `/shutdown`은 거부합니다.

MCP는 처음 읽은 주소·토큰을 실행 중 고정합니다. 토큰 회전이나 주소 변경 후에는 MCP를 재시작해야 합니다. 특히 `localPort: 0`은 브로커 재시작 시 포트가 달라질 수 있으므로 같은 주소로의 복구를 보장하지 않습니다. 별도 프로필은 별도 설정 디렉터리·포트로 분리하며, 하나의 브로커에서 Host별 자격·신원을 관리하는 제품 정책은 후속 항목입니다.

구버전에서 업그레이드할 때는 실행 중인 구버전 브로커를 구버전 CLI로 먼저 종료하세요. 새 버전은 `.devlink-runtime/`만 사용하며, 설정 파일 보호를 위해 기존 위치의 런타임·잠금 파일을 자동 이동하거나 삭제하지 않습니다. 구버전과 새 버전은 잠금 경로가 다르므로 동시에 실행하지 마세요.

## 로컬 MCP 세션

세션 생성·bind·heartbeat·해제 HTTP 경로와 90초 lease·fresh/recover 소유권·presence 연결을 구현했습니다. 2026-10-29부터 기본 CLI는 ReadyIdentityProvider로 현재 중앙 READY의 user_id/team_id/agent_id를 검증해 세션 Principal을 구성합니다. 전체 신원이 없는 구버전 READY는 연결만 유지하고 세션은 503 CENTRAL_UNAUTHENTICATED로 거부합니다. 단절 중에는 신원을 재사용해 새 세션을 만들거나 lease를 연장하지 않습니다.

세션 API는 MCP localToken만 허용하며 CLI controlToken은 거부합니다. 동일 신원 재접속은 세션을 유지하고 다른 신원은 이전 소유권을 무효화합니다. [READY 신원 계약](docs/ready-identity.md)과 [로컬 세션 API](docs/local-sessions.md)에 경로·JSON·오류·소유권·presence·검증 범위를 정리했습니다. 실제 운영 토큰 발급·업무 REST·MCP Host 왕복은 후속 연결입니다.

## 상태와 오류 처리

```text
Local Broker  : Running (PID: 12345)
Central Server: Connected
```

중앙 연결 상태는 `Connecting`, `Authenticating`, `Connected`, `Reconnecting`, `AuthFailed`, `Replaced`, `ProtocolViolation`, `Disconnected`입니다. 재연결은 초기 지연부터 최대 지연까지 지수 백오프를 사용하며, `BROKER_READY`까지 완료된 연결이 끊기면 초기 지연으로 돌아갑니다. READY 후 20초마다 JSON `HEARTBEAT` PING을 보내고, 각 요청의 같은 `ping_id` PONG을 최대 20초 기다립니다. 응답 대기 중에는 새 PING으로 기존 대기를 덮어쓰지 않습니다. 다른 JSON·이전/다른 ID의 PONG·WebSocket 제어 Ping/Pong은 이 제한을 연장하지 않습니다. 서버의 Close `4001`이나 PONG 만료는 `autoReconnect=true`이면 백오프로 재접속하고, false이면 `Disconnected`로 종료합니다. 연결 수립은 5초 제한, `BROKER_READY` 대기는 `authTimeoutMs` 제한입니다. `wss`는 OS 신뢰 저장소로 서버 인증서를 검증합니다.

브로커 실행 여부는 PID 파일 대신 OS 파일 잠금으로 판단합니다. 살아 있지만 Health Check가 실패한 프로세스는 오류로 보고하며 `Stopped`로 표시하지 않습니다. `stop`은 인증된 HTTP 종료 요청을 보내고 잠금 해제를 기다립니다. 잠금 파일은 삭제하지 않으며 정상 종료 시 `runtime.json`과 소켓·재연결 작업을 정리합니다. 강제 종료로 남은 런타임 정보는 다음 `status`/`start`/`stop`에서 잠금을 획득한 상태로 제거합니다. PID를 이용해 임의 프로세스를 강제 종료하지 않습니다.

명령/설정/시작 오류는 종료 코드 1, clap 사용법 오류는 2입니다. 이미 실행 중인 브로커의 `start`, 이미 중지된 브로커의 `stop`, 정상 `status`는 0입니다. 중앙 연결 실패는 브로커 자체의 실패가 아니므로 상태로 확인합니다.

시작 시 포트 충돌 등으로 실패하면 `devlink --config <FILE> broker`를 포그라운드에서 실행해 초기화 오류를 확인할 수 있습니다. `broker`는 내부 실행용이며 도움말에는 노출하지 않습니다.

## 코드와 테스트

| 위치 | 책임 |
| --- | --- |
| `crates/devlink-cli/src/main.rs` | CLI 진입점 |
| `crates/devlink-cli/src/cli.rs` | clap 명령·설정 관리, 자식 프로세스 실행·준비 확인·종료 대기 |
| `crates/devlink-broker/src/lib.rs` | 브로커 라이브러리 진입점 |
| `crates/devlink-broker/src/config.rs` | CLI와 브로커가 공유하는 설정 타입·검증·원자 저장·잠금 |
| `crates/devlink-broker/src/credentials.rs` | MCP 자격 파일 경로·원자 발행·토큰 유지·회전·OS 권한 검사 |
| `crates/devlink-broker/src/sessions.rs` | 신원 공급부 인터페이스·세션 소유권·90초 lease·만료·presence |
| `crates/devlink-broker/src/session_api.rs` | MCP 토큰 인증·세션 HTTP 경로·요청 검증·오류 envelope |
| `crates/devlink-broker/src/broker.rs` | HTTP 서버·종료 신호·태스크 정리 |
| `crates/devlink-broker/src/local.rs` | 런타임 발견·인증된 제어 클라이언트·Health 계약 |
| `crates/devlink-broker/src/central.rs` | 중앙 WebSocket 상태·인증·Heartbeat·재연결 |
| `crates/devlink-broker/src/protocol.rs` | 중앙 서버 메시지 DTO |
| `crates/devlink-broker/examples/mock_server.rs` | 개발용 중앙 서버 |
| `crates/devlink-cli/tests/cli_broker.rs` | 실제 CLI·브로커·Mock 서버 통합 테스트 |
| `crates/devlink-broker/tests/session_api.rs` | 테스트용 검증 신원·실제 HTTP/WebSocket 세션 통합 테스트 |

테스트는 임시 디렉터리·동적 포트를 사용하며 사용자 설정을 변경하지 않습니다. 독립 실행, 동시 시작, 중복 방지, 정상/강제 종료, 포트 충돌, 토큰 비노출, 설정 오류, 로컬 API 인증, WebSocket 인증·서버 재시작·재연결·재시도 중 종료를 검증합니다. MCP 자격 파일의 실제 Node reader 호환성, 토큰 유지·회전·권한 분리, junction 거부, 느린 ACL 검사와 presence 보고도 포함합니다. JSON Heartbeat 테스트는 같은 연결 루프에 짧은 시간을 주입해 실제 로컬 소켓에서 연결 유지·잘못된/이전 PONG·무응답 만료·4001 재접속을 검증합니다. CLI 통합 테스트는 autoReconnect의 켜짐·꺼짐과 HTTP 인증 실패 시 재접속 중단을 별도로 확인합니다. Node.js 24 이상과 MCP 패키지 의존성이 필요하며, Rust 테스트의 격리 reader 의존성이 없으면 해당 테스트가 `npm ci`를 실행합니다.

`json_heartbeat_keeps_cli_connected_for_45_seconds_without_reconnect`는 시간 단축 없이 실제 CLI의 PING 20초·PONG 대기 20초와 Mock의 JSON PING 무수신 만료 40초를 사용합니다. 45초 동안 health를 250ms 간격으로 조회하고 Connected·같은 PID/instanceId·WebSocket 연결 1회·만료 0회를 확인합니다. 약 20초·40초에 서로 다른 ping_id의 JSON PING을 같은 연결에서 받았는지와 정상 종료도 검사합니다. `#[ignore]` 없이 기본 전체 테스트에 포함되어 실행 시간이 약 45초 늘어납니다. 이 테스트만 실행하려면 `rust/`에서 다음 명령을 사용합니다.

```powershell
cargo test --offline --locked -p devlink-cli --test cli_broker json_heartbeat_keeps_cli_connected_for_45_seconds_without_reconnect -- --exact --test-threads=1
```

[중앙 WebSocket 연동 규칙](docs/central-websocket-contract.md)에 인증, READY·PRESENCE, 연결 교체·종료 규칙과 Rust 적용 현황을 정리했습니다. READY 신원·작업 수신함·ACK·업무 provider는 로컬 회귀와 고정 중앙·DB에서 검증한 기록이 있습니다. 실제 배포 환경과 Host 전체 왕복은 별도입니다.

[문서 안내](docs/README.md), [진행 현황](PROJECT_STATUS.md), [검증 요약](docs/verification.md)을 참고하세요. 현재 계약은 기능별 문서에, 과거 실습·검증 기록은 docs/archive에 보관합니다.

2026-11-02 (-143): 프로젝트 Host 설정용 [Rust McpConfig.plan/apply/inspect 공급부](../contracts/setup/v1/mcp-config-provider.md)를 제공합니다. CLI -111의 호출·통합과 구분하며 전역 설정을 쓰지 않습니다.
