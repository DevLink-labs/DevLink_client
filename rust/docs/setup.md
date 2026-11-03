# setup 명령과 공통 실행 흐름

작업 02·03·04·05·06·07·08·09·11. 2026-11-03 (Asia/Seoul). [01 호출·단계·오류 규격](../../contracts/setup/v1/README.md)을 기준으로 CLI 진입점·공통 흐름·설정 초기화·사용자 로그인·팀 선택·프로젝트 준비·도구 탐지/선택·Agent 등록/재사용·공통 브로커 제어 실행부를 구현했다.

## 사용법과 현재 범위

```powershell
devlink setup --help
devlink --config C:\work\profile\config.json setup
devlink setup --config C:\work\profile\config.json --output json
devlink setup --output jsonl
# 비밀번호는 비밀 관리 도구 등이 stdin 한 줄로 전달한다.
devlink setup --email user@example.test --password-stdin --output json
# 가입 팀이 여러 개이거나 기존 선택을 변경할 때
devlink setup --team-id 71 --output json
# 생략하면 현재 작업 폴더 사용. 공백·한글 경로는 따옴표로 묶는다.
devlink setup --project "C:\work\한글 프로젝트" --project-name "프로젝트 표시명" --agent-name "개발 Agent"
# 도구 선택. PATH 밖 설치는 실행 파일을 함께 지정할 수 있다.
devlink setup --tool codex
devlink setup --tool claude-code --tool-path "C:\tools\Claude Code\claude.exe"
# 신규 생성 요청은 최초 토큰 발급 1회까지 포함한다.
devlink setup --tool codex --create-agent
# 기존 본인 Agent 선택 또는 생성 결과 불명 복구
devlink setup --tool codex --agent-id 34
# 기존 ID의 토큰이 없거나 인증 거부된 경우에만 1회 발급 허용
devlink setup --tool codex --agent-id 34 --issue-token
```

전역 `--config`는 명령 앞/뒤 모두 기존 파서를 통해 전달되며, 공백·한글 경로도 지원한다. 생략 시 기존 `$HOME/.devlink/config.json`을 사용한다.

현재 구현은 **명령 진입점·공통 흐름·출력·설정 준비·로그인·팀/프로젝트/도구 선택·Agent 등록/재사용·토큰 검증/발급·공통 브로커 제어**다. 08 등록 provider는 Agent만 준비하고, setup의 register_agent 어댑터가 09 토큰 provider를 이어 호출한다. 정상 토큰은 실신원을 확인해 재사용한다. 새로 확인한 생성은 최초 발급 1회까지 포함하며, 기존 Agent의 누락/인증 거부 토큰은 `--issue-token`이 있어야 발급한다. 결과 불명은 알려진 ID를 보존하고 SETUP_TOKEN_REQUIRED/종료 3으로 멈춘다. MCP 설정/최종 연결 집계는 후속 작업이다. `--input`, `--non-interactive`, `setup status`는 아직 제공하지 않는다.

기본 실행기는 validate에서 프로젝트 경로를 사전 검사한 뒤 설정을 준비한다. 파일이 없으면 별도 init 없이 기본값을 비공개 권한으로 생성하고 `succeeded/applied/reused:false`로 기록한다. 유효한 기존 설정 파일은 보존하고 `succeeded/not_applied/reused:true`로 기록한다. authenticate에서 정상 세션을 서버로 확인해 재사용하거나 필요한 경우만 로그인한다. select_team에서 현재 가입 팀을 검증하고 선택한다. bind_project에서 프로젝트 디렉터리를 재검사하고 기존 Agent 프로필과 비교한 뒤 표시명·Agent명·역할을 검증한다. select_tool에서 설치된 도구를 탐지·선택한다. 이후 register_agent에서 기존 Agent를 재확인하거나 명시적 생성/ID 선택을 처리한다. 선택이 없으면 SETUP_INPUT_REQUIRED/종료 2, 생성 결과 불명이면 SETUP_REGISTRATION_UNKNOWN/종료 3, 기존 토큰의 발급 허용이 없거나 발급 결과가 불명이면 SETUP_TOKEN_REQUIRED/종료 3을 반환한다. ready 프로필은 start_broker로 진행한 뒤 아직 미연결인 configure_mcp에서 SETUP_STEP_UNAVAILABLE/종료 3으로 멈춘다. TTY도 로그인 입력도 없는 미로그인 실행은 authenticate에서 종료 4와 필요한 입력 descriptor를 반환한다. 앞서 완료한 설정 준비·확인된 세션/팀/도구 변경은 유지한다. 기존 개별 명령도 계속 사용할 수 있다.

`SETUP_STEP_UNAVAILABLE`은 작업 02의 실행 기반에 추가한 코드다. recovery.action은 `use_individual_commands`, automatic/retryable은 false이며, 단계 어댑터가 연결된 빌드 또는 기존 개별 명령을 사용하도록 안내한다. 목표 규격에 대한 이번 구현의 보충 사항은 이 rust 문서에서 관리한다.

## 코드 경계

| 위치 | 역할 |
| --- | --- |
| `devlink-cli/src/cli.rs` | setup 라우팅·기존 전역 config 경로 해석. 기존 개별 명령 로직 유지 |
| `devlink-cli/src/setup/model.rs` | 9개 Step, 단계 상태·변경 outcome, 안전한 공통 오류, 진행/최종 DTO |
| `devlink-cli/src/setup/flow.rs` | StepExecutor를 순차 호출하고 주입한 콜백으로 progress 전달 |
| `devlink-cli/src/setup/stages.rs` | 설정 준비·세션 확인·로그인 연결, 검증한 설정/사용자를 후속 단계용으로 보관 |
| `devlink-cli/src/setup/login_input.rs` | 비밀을 직렬화/Debug하지 않는 입력 DTO와 주입 가능한 입력 인터페이스 |
| `devlink-cli/src/setup/team_input.rs` | 팀 ID 검증과 주입 가능한 팀 선택 입력 인터페이스 |
| `devlink-cli/src/setup/tool_input.rs` | 탐지된 도구 선택 입력 인터페이스 |
| `devlink-cli/src/setup/terminal.rs` | clap 옵션·human/JSON/JSONL 렌더링·프로세스 종료 코드 전달 |
| `devlink-cli/src/terminal_input.rs` | 기존 auth/team 명령과 setup이 공유하는 숨김/전용 stdin 입력 |
| `devlink-broker/src/user_auth.rs` | 기존 로그인·CSRF·세션 저장과 typed session check·장애 분류 |
| `devlink-broker/src/teams.rs` | 현재 사용자·목록·상세·멤버십 검증과 선택 저장 |
| `devlink-broker/src/agent_profile.rs` | 기존/미확정 Agent 바인딩의 사용자·서버·팀 충돌 확인 |
| `devlink-broker/src/setup_config.rs` | 설정 경로·형식·값·자격 파일 안전성·브로커 잠금 검사 |
| `devlink-broker/src/setup_project.rs` | 프로젝트 경로 정규화·표시명 기본값·기존 프로젝트 비교, 등록 메타데이터와 로컬 경로 분리 |
| `devlink-broker/src/tool_discovery.rs` | 설치 위치 탐색·버전 전용 실행·재사용 가능한 실행 정보·MCP 제공자 연결 |
| `devlink-broker/src/setup_tools.rs` | 사용자/팀/프로젝트에 묶인 비공개 도구 선택 저장·재사용·경합 검사 |
| `devlink-broker/src/broker_control.rs` | 공통 시작·중지·상태 조회·중지 상태 잠금·소유 자식 정리. CLI와 setup이 공유 |
| `devlink-broker/src/config.rs` | 기존 init과 setup이 공유하는 초기화·원자적 저장·잠금 |
| `devlink-cli/src/lib.rs` | `devlink_cli::setup`을 테스트와 다른 호출자에서 재사용하도록 공개 |

로그인 입력은 LoginInput 인터페이스로 주입한다. 인증 로직은 필요한 입력 descriptor를 반환하며, 터미널 어댑터가 숨김 입력 또는 전용 stdin을 처리한다. 공통 flow와 인증 로직에서 직접 `stdin/read_line/prompt/println`을 호출하지 않는다. 기존 CLI 명령을 자식 프로세스로 실행해 stdout을 분석하지 않는다.

## 단계 실행기·진행 구독

호출자는 config_path가 든 SetupContext, StepExecutor 구현, 진행 콜백을 넘긴다. StepExecutor는 Complete/Failed/InputRequired를 반환한다. 입력을 처리할 수 있는 어댑터가 있으면 resolve_input을 한 번 호출하고 해당 단계만 재개한다. 대화형에서는 waiting_input과 필요한 필드만 진행 이벤트로 알린다. 이전 단계는 다시 실행하지 않으며 로그인/네트워크 실패 때문에 요청을 자동 재전송하지 않는다. 전체 성공 데이터는 모든 단계가 통과한 뒤에만 finish에서 꺼낸다. 자격 값은 공개 결과에 담지 않는다.

```rust,ignore
let result = devlink_cli::setup::run(&context, stage_executor, |event| {
    event_sender.send(event.clone())
        .map_err(|_| devlink_cli::setup::ProgressUnavailable)
}).await;
```

실행 순서는 validate → authenticate → select_team → bind_project → select_tool → register_agent → start_broker → configure_mcp → verify_connection이다. 각 단계의 running/최종 상태를 같은 runId와 증가하는 sequence로 전달한다. 전체 성공은 18개 progress와 sequence=19의 result다. 콜백을 비동기 UI/채널에 연결할 때는 콜백 안에서 입력을 기다리지 않고 이벤트를 전달한다.

실패 시 현재 단계는 failed, 이후 단계는 skipped다. 자동 재실행·등록/발급 재시도는 하지 않는다. 생성 결과 unknown과 확인한 ID, 앞 단계의 applied outcome은 그대로 보존한다. 진행 수신자가 끊기면 다음 단계를 시작하지 않는다. 변경 후 전달이 실패해도 이미 적용한 결과는 유지하며 되돌림을 추측하지 않는다. 도메인 오류와 진행 전달 실패가 함께 발생하면 원래 도메인 오류를 보존한다.

실행기가 Complete와 outcome=unknown을 함께 반환한 경우에도 다음 단계로 넘어가지 않는다. 해당 단계를 failed/unknown으로 기록하고 SETUP_INTERNAL_ERROR(종료 1)를 반환한다. 결과 불명을 적용 전 실패나 성공으로 바꾸지 않는다.

json은 최종 result만, jsonl은 progress 뒤 result 한 개를 출력한다. human은 단계와 고정 오류 문구를 출력한다. 최종 result는 data/error 중 하나만 갖는다. renderer는 주입한 Write에 기록하므로 메모리 버퍼 등으로 출력할 수도 있다. machine 결과를 출력한 뒤 main에서 같은 오류를 다시 stderr에 출력하지 않는다. 잘못된 setup 인자는 값을 echo하지 않고 종료 2로 처리하며 기존 명령의 도움말·파서 오류는 유지한다.

## 재현

rust 디렉터리에서 실행한다.

```powershell
cargo test --offline --locked -p devlink-cli --test setup_flow --test cli_setup --test setup_output --test setup_configuration --test setup_authentication --test setup_teams --test cli_auth --test cli_teams -- --test-threads=1
cargo test --offline --locked -p devlink-cli --test cli_broker setup_validates_existing_config_without_changing_a_running_broker -- --test-threads=1
cargo test --offline --locked --workspace --all-targets -- --test-threads=1
cargo fmt --all -- --check
cargo clippy --offline --locked --workspace --all-targets -- -D warnings
```

추가 회귀는 순차 성공·같은 config 전달·외부 진행 구독·등록 결과 불명에서 중단·추가 입력을 대기 없이 반환·진행 수신 중단 전후의 변경 결과·도메인 오류 보존, 실제 CLI help/JSON/JSONL/종료 코드·전역 config·기존 명령·setup 인자 비노출을 검사한다. 실제 중앙 등록이나 Host 업무 왕복 완료를 뜻하지 않는다.

## 2026-11-02 검증 결과

client `02dfe9d` 기반 작업 트리, Windows stable MSVC에서 확인했다. 아래 표는 최초 구현 검증이며 이후 검토 보완의 재검증과 구분한다.

| 검사 | 결과 |
| --- | --- |
| 새 setup 회귀 | CLI 5개 + 공통 흐름 6개 통과. 아래 전체 집계에도 포함 |
| 전체 Rust workspace/all-targets | 180 passed / 0 failed / 기존 opt-in 2 ignored |
| fmt / Clippy | 전체 fmt check와 workspace/all-targets Clippy `-D warnings` 종료 0 |
| 문서·변경 검사 | 로컬 문서 링크 45개 확인, `git diff --check` 통과 |

첫 샌드박스 실행에서는 기존 init의 원자적 파일 교체가 권한 제한으로 실패했다. 승인된 샌드박스 외부의 임시 프로필 환경에서 재실행해 정상 동작을 확인하고 전체 회귀도 같은 환경에서 통과했다. 초기 검사에서 Windows 도움말의 `.exe` 표기와 clap invalid-value 오류의 값 노출을 발견해 보완했다. 컴파일러의 incremental cache hard-link 복사 대체 경고는 발생했으며 코드 lint 실패와 구분한다. 중앙/DB·실제 Host 업무 왕복을 추가 실행한 결과는 아니다.

## 검토 후 보완

Complete/unknown이라는 모순된 단계 결과를 성공으로 받아들여 다음 단계까지 실행하는 문제를 회귀 테스트로 재현했다. 공통 흐름에서 failed/unknown으로 중단하도록 수정하고 이전 적용 결과와 결과 불명 상태를 유지한다. 이 분기는 단계 어댑터의 잘못된 반환에 대한 검사이며, 실제 등록 성공/실패를 추정하지 않는다.

보완 후 setup CLI 5개와 흐름 7개, 총 12개가 통과했다. 전체 fmt check와 workspace/all-targets Clippy `-D warnings`도 재검증했다. 앞선 180개 전체 회귀 결과와 합산하지 않으며 보완 후 전체 회귀를 다시 실행한 결과로 표시하지 않는다. 나머지 진입점·config 전달·출력 채널·실패 시 후속 중단 경계에서는 추가 수정 사항을 찾지 못했다. 실제 입력·등록·MCP 설정 단계의 연결은 계속 후속 범위다.

사용자 요청에 따라 이번 작업의 rust 외부 문서 변경은 복원했다. 후속 수정은 rust 안에서 진행하고 외부 파일 변경이 필요하면 먼저 허락을 받는다.

## 추가 경계 검토와 최신 재검증

실행 흐름·공통 DTO·CLI 파서·종료 코드·출력 어댑터를 다시 검토했다. Complete/unknown 차단 수정을 포함해 추가 실행 로직 결함은 발견하지 못했다. 검증 범위를 보강해 CLI 회귀 1개와 출력 회귀 4개를 추가하고, 기존 인자 비노출 테스트를 5가지 호출 경로로 확장했다.

- 전역 config가 앞/뒤에 오거나 `--config=...`인 경우, 잘못된 output 값 및 위치 인자의 표식 값이 stdout/stderr에 노출되지 않는다.
- config 값 자체가 `setup`일 때 기존 Agent 명령의 필수 인자 오류를 setup 오류로 잘못 분류하지 않는다.
- JSON은 최종 결과 한 줄만 출력하고 JSONL은 진행과 결과를 별도 레코드로 출력한다.
- 제어/bidi 문자를 escape해 표시 변조를 막고, JSON을 다시 읽으면 원래 한글·개행·문자 값을 보존한다.
- 출력 연결 중단은 ProgressUnavailable로 전달하고 원본 I/O 진단을 노출하지 않는다.
- 직렬화 오류는 부분 JSON이나 원본 진단을 출력하지 않는다.

| 최신 검사 | 결과 |
| --- | --- |
| setup CLI | 6 passed |
| 공통 실행 흐름 | 7 passed |
| 출력 어댑터 | 4 passed |
| 관련 회귀 합계 | 17 passed / 0 failed |
| 전체 fmt / workspace/all-targets Clippy `-D warnings` | 종료 0 |

관련 테스트는 승인된 Windows 임시 프로필 환경에서 실행했다. 이 17개는 앞선 12개를 포함하며 이전 전체 180개 결과에 더하지 않는다. 이번 추가 검토에서는 전체 기능 회귀·실제 중앙/DB·실제 Host 업무 왕복을 다시 실행하지 않았다. 파일 수정은 rust 안에서만 수행했고 실명을 추가하지 않았다.

## 작업 03: 설정 초기화와 기존 파일 확인

validate의 파일·권한 검사는 spawn_blocking에서 실행해 비동기 실행 흐름의 스레드에서 터미널 입력이나 동기 ACL 검사를 기다리지 않는다. 경로는 파일을 가리키는 절대 경로여야 하고 `.devlink-runtime` 내부는 거부한다. 파일·부모·상위 경로의 symlink/Windows junction·reparse point를 확인한 뒤 디렉터리와 잠금을 준비한다.

설정에는 비밀값이 없으므로 다른 사용자의 읽기 권한이 있는 기존 init·수동 작성 config도 허용한다. 다만 serverUrl이 비밀번호 전송 대상을 결정하므로 Windows config 소유자는 현재 사용자 또는 BUILTIN\Administrators(S-1-5-32-544)여야 한다. 관리자 셸에서 init으로 만든 config도 이 기준으로 허용한다. 신뢰 주체(현재 사용자·SYSTEM·Administrators) 외의 쓰기·삭제·권한 변경 허용과 부모 디렉터리의 같은 권한은 SETUP_UNSAFE_FILE로 거부한다. Unix에서는 현재 사용자 소유만 허용하고 group/other 쓰기 비트(0o022)를 거부한다. config는 일반 파일 여부와 16 KiB 상한을 검사하고 비공개 snapshot과 같은 제한 읽기 함수로 읽는다. JSON/스키마·중복 키·알 수 없는 필드·값 형식을 검사하고 Config 검증이 반환하는 필드 정보를 진단에 사용한다. 사용자 인증 origin 검증도 재사용하므로 원격 중앙은 wss, ws는 localhost/loopback만 허용한다. agent·포트·재연결·인증 제한시간도 검증한다. 기존 3필드 설정은 기본값을 메모리에 보충하며 파일을 재작성하지 않는다. 세션·Agent·팀 선택·MCP 자격 파일에는 현재 사용자 소유를 요구하는 기존 비공개 권한 검사를 유지한다.

새 부모 디렉터리만 비공개 권한으로 준비한다. 기존 디렉터리/파일의 권한은 자동 수정하지 않는다. 새 설정은 기존 init이 사용하는 기본값·같은 디렉터리 임시 파일·flush 경로를 공유하고, 설정 생성은 persist_noclobber로 다른 생성 결과를 덮어쓰지 않는다. setup의 임시 파일에는 기록 전 비공개 권한을 적용한다. 생성이 성공하면 메모리의 기본값을 재사용한다. persist 이전 실패는 not_applied이며, persist 이후 부모 디렉터리 동기화 실패만 unknown으로 보고한다. 기존 init도 같은 생성 함수를 사용하며 이미 존재하는 설정을 보존한다.

setup은 config 대상과 모든 상위 경로의 symlink·Windows junction/reparse point를 계속 거부한다. macOS의 `/tmp`·`/var` 또는 링크된 `/home` 아래도 이 제한을 받으므로 `--config`에 링크를 해석한 실제 경로를 지정해야 한다. 이 정책은 setup의 경로 검사에 적용하며, 기존 init은 유효한 config를 가리키는 심볼릭 링크를 종전처럼 읽고 보존한다.

control.lock을 확보한 뒤 broker.lock을 확인한다. 다른 제어 작업이 있으면 CONFIG_BUSY다. broker.lock이 점유됐으면 실행 중/시작 중으로 취급한다. 기존 정상 설정은 읽기 검증만 허용하며 누락 설정 생성은 BROKER_BUSY로 중단한다. health/PID만으로 실행 여부를 추정하거나 브로커를 중지하지 않는다. 중지 상태에서는 broker.lock도 유지해 초기화 중 직접 브로커 기동과 충돌하지 않게 한다. 상태 조회 도중 stale runtime 파일을 삭제하는 기존 local::running은 이 단계에서 호출하지 않는다.

기존 user-session/selected-team/agent-profile 파일이 있으면 private reader로 크기·권한·JSON 안전성을 확인한다. MCP 자격 파일은 credentials::load 한 번으로 크기·권한·스키마를 검증한다. 자격을 발급·회전·삭제하지 않고 원문을 출력하지 않는다. 사용자·팀·Agent의 권한/세션 유효성 확인은 후속 단계 책임이며 이 저장소 검사가 중앙 인증 성공을 뜻하지 않는다.

오류에는 step=validate, location.file과 가능한 location.field, 고정 문구·복구 action·outcome을 포함한다. human 출력에도 실패 단계·위치·복구 action을 표시하고 경로의 제어/bidi 문자를 escape한다. 원본 JSON·URL의 비밀번호/토큰·provider 오류 본문은 전달하지 않는다.

유효한 유니코드로 표현할 수 없는 OS 경로 단위는 오류의 표시 문자열에서 대체 문자로 바꾼다. 파일 접근에 쓰는 실제 경로는 바꾸지 않는다. 이 경우에도 오류 JSON·실패 위치·복구 action을 출력할 수 있는지 회귀로 확인한다.

| 코드 | 종료 | 복구 action |
| --- | --- | --- |
| SETUP_CONFIG_INVALID | 2 | repair_config_then_retry: 표시한 기존 파일/필드를 수동 수정 |
| SETUP_CONFIG_PATH_INVALID | 2 | choose_config_path: 내부 runtime 밖 설정 파일 지정 |
| SETUP_UNSAFE_FILE | 5 | repair_private_file: 경로·소유권·권한·크기·내용 확인 |
| SETUP_CONFIG_BUSY | 6 | wait_for_config_operation: 기존 제어 작업 종료 후 재시도 |
| SETUP_BROKER_BUSY | 6 | stop_owned_broker_then_retry: 해당 프로필 브로커 중지 후 누락 파일 생성 |
| SETUP_CONFIG_STORAGE_FAILED | 5 | inspect_config_before_retry: 경로/파일과 저장 결과부터 확인 |

초기화 중 persist 이전 저장 오류는 not_applied, persist 이후 부모 디렉터리 동기화 오류는 unknown으로 기록하며 파일을 삭제하지 않는다. 손상 파일을 자동 초기화하거나 재생성하지 않는다.

작업 03의 수용 검사는 최초 자동 생성·두 번째 실행 내용/수정 시각 보존·기존 3필드 설정·잘못된 JSON/URL/범위·초과 크기·불안전한 파일 권한·control/broker 잠금·동시 setup·junction 상위 경로·기존 MCP 토큰 보존·human 복구 안내를 포함한다. 실제 브로커를 실행한 검사에서는 PID·설정·MCP 파일·runtime 원문이 모두 보존됐는지 대조한다. 테스트는 임시 config를 명시해 실제 사용자 기본 설정을 생성하지 않는다.

### 작업 03 검증 결과

Windows stable MSVC, 임시 프로필·합성 중앙 fixture 기준이다.

| 검사 | 결과와 범위 |
| --- | --- |
| 공유 초기화/잠금 변경의 전체 workspace/all-targets | 199 passed / 0 failed / 기존 opt-in 2 ignored. 오류 경로 표시 보완 이전 실행 |
| 최신 setup 관련 회귀 | CLI 6 + 설정 12 + 흐름 7 + 출력 5 = 30 passed |
| 최신 실제 브로커 보존 회귀 | 1 passed. PID·설정·MCP·runtime 보존 |
| 최신 fmt / workspace/all-targets Clippy `-D warnings` | 종료 0 |

처음 검사에서 새 설정 디렉터리의 비공개 권한 준비 누락을 발견해 수정했다. 이어서 오류 위치에 유니코드로 표현할 수 없는 OS 경로가 있으면 일반 Path 직렬화가 실패하는 경계를 보완하고 관련 테스트에서 기본 직렬화 실패와 보완 후 정상 응답을 대조했다. 보완 후에는 위 30개와 실제 브로커 1개를 재실행했으며 전체 199개 결과와 합산하지 않는다. 초기화·검증·브로커 변경 가능 여부에 대한 작업 03 기준은 충족한다. 실제 로그인·등록·Host 연결까지 완료한 결과는 아니다.

## 작업 04: 로그인 상태 확인과 단계 통합

authenticate는 먼저 기존 사용자 세션을 확인한다. 로그인 정보가 있으면 기존 사용자 인증 모듈의 `/api/v1/auth/users/me`로 실제 신원을 검증한다. 동일한 사용자 ID가 확인돼야 세션을 재사용하고 이메일/비밀번호 입력 어댑터를 호출하지 않는다. `--password-stdin`을 지정했더라도 정상 세션이면 stdin을 읽지 않는다. 검증 후 갱신한 공개 사용자·쿠키 스냅샷은 기존 private writer로 저장하므로 `reused:true`와 `outcome:applied`가 함께 올 수 있다.

미로그인·로컬 expired 상태 또는 서버에서 확인한 HTTP 401 만료만 로그인 입력을 요청한다. 401이면 기존 로직대로 cookie를 제거한 expired 스냅샷을 먼저 저장하며 입력 취소/실패에도 이 변경을 applied로 유지한다. 네트워크/본문 수신 실패·5xx·429·408은 CENTRAL_UNAVAILABLE이며 기존 세션을 만료로 바꾸거나 자격을 다시 요청하지 않는다. 403은 인증 거부, 잘못된 응답/CSRF/쿠키·신원 불일치는 PROTOCOL_ERROR다. 원본 응답과 오류 문구는 복사하지 않는다.

대화형 입력은 주입한 LoginInput 어댑터가 맡는다. 이메일을 입력받고 기존 숨김 비밀번호 reader를 호출한다. 입력이 없는 실행 환경에서는 대기하지 않고 AUTH_REQUIRED와 필요한 필드를 반환한다. 자동화는 `--email <EMAIL> --password-stdin`을 사용하며 비밀번호 한 줄을 stdin으로 전달한다. 이메일과 비밀번호를 얻은 뒤에도 세션을 다시 확인하고, 입력 대기 중 다른 프로세스가 정상 로그인했다면 그 세션을 재사용한다.

입력이 필요한 단계는 InputRequired를 반환한다. 처리 가능한 어댑터가 있을 때 resolve_input을 호출하고 해당 단계만 재개한다. 대화형이면 waiting_input 이벤트에 필드 descriptor만 포함하며 stdin 자동화는 별도 대화형 대기 상태를 만들지 않는다. 한 단계의 입력 해결은 최대 한 번이며 로그인 실패 후 자동 재입력·재전송은 없다. 진행 수신자가 입력 전에 끊기면 입력 어댑터도 호출하지 않는다. 입력 취소는 cancelled/종료 130이며 이전 적용 상태를 보존한다. 강제 종료·OS 신호로 출력 자체가 불가능하면 최종 JSON 전달까지 보장하지 않는다.

로그인은 기존 Rust의 CSRF 조회 → 로그인 POST → `/users/me` 신원 일치 확인 → 회전된 CSRF 확인 → private 세션 저장 경로를 공유한다. account.lock 안에서 현재 세션 확인과 필요 시 로그인을 수행해 정상 세션을 불필요하게 교체하지 않는다. 검증한 설정의 중앙 origin을 확인/로그인 호출에 고정하고, 입력 중 서버가 바뀌면 요청을 보내기 전에 SESSION_CONFLICT로 중단한다. origin 불일치나 손상된 로컬 세션도 자동 입력/덮어쓰기로 복구하지 않으며 명시적 auth login 등으로 먼저 확인한다.

LoginCredentials에는 Debug/Serialize를 구현하지 않는다. 비밀번호는 입력부터 호출까지 메모리에서만 전달하며 CLI 인수·환경변수·설정·Agent 프로필·로그·진행/결과 JSON에 저장하지 않는다. 기존 auth/team 명령과 setup의 비밀 입력을 terminal_input으로 공유했다. LF/CRLF 줄 끝만 제거하며 비밀번호 공백은 보존한다. 사용자 cookie·CSRF·Agent/MCP 자격은 기존 분리를 유지한다.

| 코드 | 종료 | 복구 action |
| --- | --- | --- |
| SETUP_AUTH_REQUIRED | 4 | login: 숨김 입력 또는 --email/--password-stdin 제공 |
| SETUP_AUTH_REJECTED | 4 | login: 계정/자격 확인 후 명시 재시도 |
| SETUP_CENTRAL_UNAVAILABLE | 7 | check_network_then_retry: 연결·서비스 복구 후 재시도 |
| SETUP_PROTOCOL_ERROR | 7 | check_provider_contract: 응답·신원·CSRF/쿠키 확인 |
| SETUP_AUTH_SESSION_CONFLICT | 6 | inspect_profile_then_reauthenticate: 중앙 origin/프로필 확인 |
| SETUP_AUTH_BUSY | 6 | wait_for_auth_operation: account.lock 소유 작업 종료 대기 |
| SETUP_CONFIG_BUSY | 6 | wait_for_config_operation: 로그인 발행 전 control.lock 소유 작업 종료 대기 |
| SETUP_BROKER_BUSY | 6 | stop_owned_broker_then_retry: 사용자 전환 전 실행/시작 중인 브로커 중지 |
| SETUP_CONFIG_INVALID | 2 | repair_config_then_retry: 인증 중 다시 읽은 config의 표시 필드 확인 |
| SETUP_CONFIG_STORAGE_FAILED | 5 | inspect_config_before_retry: 인증 중 config를 읽지 못함 |
| SETUP_UNSAFE_FILE | 5 | repair_private_file: 사용자 전환 중 control.lock/broker.lock이 불안전하거나 열 수 없음 |
| SETUP_AUTH_STORAGE_FAILED | 5 | inspect_private_session: 세션 파일·권한 확인 |
| SETUP_CANCELLED | 130 | inspect_status_before_resume: 완료 단계 확인 후 재개 |

브로커/제어 잠금 충돌·잠금 파일 오류와 config 재읽기 실패는 세션 저장 오류로 분류하지 않고 해당 파일 위치와 not_applied를 반환한다. config 값 오류는 Config 검증의 필드(JSON 오류는 `$`)를, 읽기 I/O 오류는 SETUP_CONFIG_STORAGE_FAILED를 반환한다. 로그인·팀 선택 입력의 I/O 실패는 SETUP_INTERNAL_ERROR로, 비밀번호 입력의 EOF·입력 중단은 SETUP_CANCELLED로 구분한다. 빈 비밀번호 줄이나 길이·형식 위반은 SETUP_INPUT_INVALID다. 같은 입력 함수를 쓰는 auth/team 명령은 빈 stdin에서 "No Password was received on stdin"처럼 누락된 값을 안내한다. 앞서 확인된 세션 만료 등 이미 적용한 변경은 공통 흐름에서 계속 보존한다.

인증 성공 시 검증한 User를 후속 단계용으로 보관한다. 작업 04 종료 당시에는 select_team이 STEP_UNAVAILABLE/종료 3이었다. 작업 05에서 팀 선택, 작업 06에서 프로젝트 준비, 작업 07에서 도구 선택을 연결했으며 현재 중단 단계는 register_agent다. 회원가입·Agent 등록·Host 전체 연결은 후속 작업이다.

```powershell
# rust 디렉터리에서 인증 통합과 기존 인증 회귀 확인
cargo test --offline --locked -p devlink-cli --test setup_authentication --test cli_auth --test setup_flow -- --test-threads=1
```

인증 검사는 정상 세션의 열린 stdin 비소비·미로그인/만료 로그인·서버 신원 확인·실패 시 snapshot 보존·네트워크/5xx/403/프로토콜 분류·비밀 비노출·입력 중 타 프로세스 로그인/서버 변경·계정 잠금·입력 취소를 포함한다. 입력 요청은 주입한 어댑터로 검사하고 실제 숨김 입력에는 기존 rpassword reader를 사용한다. 합성 중앙·임시 프로필 검증을 실제 운영 중앙/DB나 Host 업무 왕복 완료로 확대하지 않는다.

### 작업 04 검증 결과

2026-11-02, Windows stable MSVC에서 최종 소스를 검사했다. 합성 중앙 서버·임시 비공개 프로필·실제 CLI 자식 프로세스를 사용했으며 기존 사용자 기본 설정을 변경하지 않았다.

| 검사 | 결과 |
| --- | --- |
| 전체 workspace/all-targets | 221 passed / 0 failed / 기존 opt-in 2 ignored |
| 새 setup 인증 통합 | 18 passed |
| 기존 인증 / Agent / 팀 / 브로커 회귀 | 각각 8 / 12 / 5 / 35 passed |
| setup CLI / 설정 / 흐름 / 출력 | 각각 6 / 12 / 10 / 5 passed |
| `cargo fmt --all -- --check` | 종료 0 |
| workspace/all-targets Clippy `-D warnings` | 종료 0 |
| `devlink setup --help` | 종료 0, 이메일·전용 stdin 옵션과 현재 범위 확인 |

개별 테스트 수는 전체 221개에 포함되며 합산하지 않는다. 제외된 2개는 Node 24 이상·현재 MCP 빌드와 별도 실행 조건이 필요한 기존 MCP 통합 검사다. 실제 운영 중앙/DB·Host 전체 왕복 및 실제 대화형 콘솔의 숨김 입력 수동 검증은 수행하지 않았다.

인증 오류 분류·입력 재개·동시 로그인·중앙 origin 변경·저장/출력 경계를 검토했다. 작업 04의 완료 기준은 위 자동 검증 범위에서 충족하며, 인증 후 팀 선택은 후속 작업이다. 변경 파일은 모두 rust 내부이고 실명을 추가하지 않았다.

## 작업 05: 팀 조회·선택과 재사용

select_team은 인증 단계에서 확인한 사용자 ID와 중앙 origin을 고정한다. 기존 사용자 클라이언트의 account.lock 안에서 로컬 세션과 서버 `/users/me`를 다시 확인하고 가입 팀 목록을 조회한다. 입력 중 다른 사용자로 로그인하거나 서버가 바뀌면 추가 서버 요청 전에 SESSION_CONFLICT로 중단한다.

선택 우선순위는 명시한 `--team-id` → 같은 origin/사용자의 유효한 기존 선택 → 가입 팀이 하나일 때 자동 선택이다. 이전 사용자/서버의 선택이나 현재 목록에서 사라진 팀은 재사용하지 않는다. 가입 팀이 여러 개면 TeamInput 어댑터에 선택을 요청하며, 대화형 입력이 없으면 종료 2와 `team.id/selection/false/verified_team_list` descriptor, 공개 `teamChoices`를 반환한다. ID는 정규 양의 십진수이고 최댓값은 9223372036854775807이다. machine 응답의 ID는 문자열로 유지한다.

팀이 없으면 종료 3과 `devlink team create --name <NAME>` / `devlink team join` 안내를 반환한다. setup이 팀 생성·참여나 초대코드 입력을 자동 수행하지 않는다. 이전 선택은 삭제하지 않는다.

선택한 ID가 현재 목록에 있어야 하고 기존 team select와 같은 상세·멤버 목록 검사로 현재 사용자·역할을 확인한다. 목록의 중복/잘못된 ID, 상세의 ID/역할 불일치도 거부한다. 입력 후에는 account.lock을 다시 얻고 목록·상세·멤버십을 재조회하므로 화면에 보였던 후보만으로 진행하지 않는다. 서버 장애와 프로토콜 오류는 빈 팀 목록이나 세션 만료로 바꾸지 않는다. 서버가 확인한 401은 기존 인증 로직대로 쿠키를 제거하며 setup을 명시적으로 다시 실행하도록 안내한다.

선택 저장 직전 control.lock 안에서 설정을 다시 읽어 중앙 origin을 대조한 뒤 기존 Agent 프로필을 검사한다. 멤버십 응답 대기 중 config set으로 서버가 변경되면 SESSION_CONFLICT/종료 6으로 중단하고 config 위치를 표시한다. 이 잠금은 새 선택 저장과 기존 선택 재사용 성공 반환까지 유지한다. ready·token_required·생성 미확정 상태가 다른 origin/사용자/팀에 묶여 있으면 충돌로 중단한다. 같은 바인딩과 vacant 상태는 허용한다. Agent/MCP 자격 발급·회전·삭제, 브로커 중지/재시작은 하지 않는다. 개별 team select 명령의 기존 동작은 유지한다.

새 선택은 기존 private writer로 selected-team.json에 origin/userId/teamId만 원자 저장한다. 정상 선택 재사용은 파일 내용과 수정 시각을 보존하며 `reused:true/outcome:not_applied`다. 새 선택은 `reused:false/outcome:applied`이고 저장 결과가 불확실하면 unknown으로 멈춘다. 손상된 선택·Agent 파일은 자동 삭제하거나 덮어쓰지 않는다.

대화형 선택은 waiting_input 진행 이벤트에 descriptor와 teamChoices만 전달하고 해당 단계만 재개한다. 후보 이름은 기존 출력 어댑터의 제어/bidi 문자 escape를 적용한다. 입력 취소는 종료 130이며 인증 단계의 완료 상태를 보존한다. 후속 단계에 검증한 Team을 보관하고 결과의 known.userId/teamId를 전달한다. 작업 05 종료 당시에는 bind_project에서 종료 3이었으며 작업 06에서 프로젝트 준비를 연결했다.

| 코드 | 종료 | 복구 action |
| --- | --- | --- |
| SETUP_TEAM_REQUIRED | 2 | select_verified_team: --team-id 또는 터미널 선택 |
| SETUP_TEAM_MEMBERSHIP_REQUIRED | 3 | create_or_join_team_then_retry |
| SETUP_TEAM_FORBIDDEN | 4 | select_verified_team: 현재 가입 팀과 멤버십 확인 |
| SETUP_TEAM_LIST_RESOURCE_NOT_FOUND | 7 | inspect_central_team_data: 서버 관리자에게 팀·가입 정보 확인 요청 |
| SETUP_TEAM_BINDING_CONFLICT | 6 | inspect_agent_binding: 기존 Agent 확인 후 명시적 바인딩 또는 별도 프로필 선택 |
| SETUP_TEAM_STORAGE_FAILED | 5 | inspect_team_selection: 선택/Agent 파일·권한·저장 결과 확인 |

네트워크·프로토콜·인증·계정/프로필 잠금 오류는 기존 setup 코드를 재사용한다. 다음은 여러 팀 중 선택이 필요한 오류의 필드 발췌다.

팀 목록 `GET /api/v1/teams`의 `404 RESOURCE_NOT_FOUND`는 가입 정보에 연결된 팀을 찾을 수 없는 응답이므로 `SETUP_TEAM_LIST_RESOURCE_NOT_FOUND`로 구분한다. 서버 관리자에게 팀·가입 데이터 확인을 안내하며 권한 오류나 빈 목록으로 바꾸지 않는다. 종료 코드는 7, outcome은 not_applied, retryable과 automatic은 false다. 로컬 파일 오류가 아니므로 location은 생략하고 기존 선택과 인증 상태를 보존한다. 상세·멤버 조회에서 발생한 403/404의 기존 멤버십 미확인 처리는 유지한다. 중앙 API 응답 형식과 개별 `team list`의 `RESOURCE_NOT_FOUND` 출력은 유지한다.

```json
{
  "code": "SETUP_TEAM_REQUIRED",
  "step": "select_team",
  "requiredInputs": [
    {"field": "team.id", "kind": "selection", "secret": false, "source": "verified_team_list"}
  ],
  "teamChoices": [
    {"id": "71", "name": "Team A"},
    {"id": "72", "name": "Team B"}
  ]
}
```

### 작업 05 검증 결과

2026-11-02, Windows stable MSVC·합성 중앙 서버·임시 비공개 프로필 기준이다. 실제 CLI 자식 프로세스와 주입한 TeamInput 어댑터를 사용했다.

| 검사 | 결과 |
| --- | --- |
| 새 setup 팀 조회·선택·재사용 | 15 passed |
| setup 로그인 통합 / 기존 인증 CLI / 기존 팀 CLI | 각각 18 / 8 / 5 passed |
| setup CLI / 설정 / 공통 흐름 / 출력 | 각각 6 / 12 / 10 / 5 passed |
| 기존 Agent 팀 전환 / 사용자 전환 / 실행 중 브로커 보존 | 각각 1 passed |
| 위 관련 회귀 합계 | 82 passed / 0 failed, 중복 실행 제외 |
| 최종 fmt / workspace/all-targets Clippy `-D warnings` | 종료 0 |
| help / 변경 공백 / 수정 범위 / 로컬 문서 링크 | 정상, 변경은 rust 내부 |

팀 검사에는 단일 팀 자동 선택·정상 선택 파일/수정 시각 보존·이전 사용자/서버/탈퇴 팀 선택 배제·다중 팀 입력 반환·명시 선택·팀 없음 안내·상세/멤버십 미확인 차단·입력 중 가입 해제/사용자/서버 변경·취소·계정/프로필 잠금·손상 선택 보존·정규 ID와 signed-long 최댓값·서버 장애/중복 응답/401·Agent ready/생성 미확정 충돌과 자격 보존을 포함한다.

초기 검사에서 테스트용 미확정 Agent 프로필의 필드명이 실제 저장 형식과 다르던 부분과 이전 도움말을 기대하던 검사를 수정했다. 최종 검토에서 가입 팀이 없으면 명시 team ID가 있어도 생성/참여를 안내하도록 보완하고 해당 1개를 다시 실행했다. 이 재실행은 82개에 더하지 않는다. 관련 실행 로그는 rust/target/setup05-*.log에 보관하며 빌드 산출물이므로 Git에 추가하지 않는다.

이번에는 전체 workspace 테스트를 재실행하지 않았으며 변경 경로의 위 회귀와 전체 타깃 정적 검사를 수행했다. 실제 운영 중앙/DB·Host 전체 왕복·실제 대화형 콘솔 수동 검증은 포함하지 않는다. 검증 범위에서 작업 05 완료 기준을 충족하며 프로젝트·도구·Agent 준비와 MCP 연동은 후속 작업이다. 실명을 추가하지 않았다.

### 작업 05 후속 검토: 멤버십 응답 대기 중 서버 변경

기존 검사는 입력 대기 중 서버 변경을 확인했지만, 멤버십 HTTP 응답을 기다리는 동안의 변경을 놓쳤다. account.lock은 유지돼도 config set은 별도 control.lock으로 설정을 변경할 수 있었다. 이전 구현은 응답 이후 현재 서버를 다시 확인하지 않아 이전 서버의 선택 파일을 저장하거나 재사용 성공을 반환했다.

합성 중앙의 멤버십 응답을 Notify로 대기시키고 실제 config set 명령으로 서버를 변경해 재현했다. 수정 전에는 선택 파일이 없던 프로필에 이전 서버의 선택이 생성돼 테스트가 실패했다. 이제 control.lock 획득 후 설정과 정규화한 중앙 origin을 다시 확인하고, 달라졌으면 SETUP_AUTH_SESSION_CONFLICT/종료 6·outcome:not_applied·config 파일 위치를 반환한다. 해당 잠금은 저장과 재사용 성공 반환까지 유지한다.

수정 후 신규 선택 파일 미생성과 기존 선택 파일 보존을 같은 경합 테스트의 두 사례로 확인했다. 팀 회귀 전체는 16 passed / 0 failed이며 fmt와 workspace/all-targets Clippy `-D warnings`도 종료 0이다. 앞선 82개와 합산하지 않는다. 이번 검토에서는 전체 workspace 테스트를 재실행하지 않았다. 재현 전후와 회귀 로그는 rust/target/setup05-server-race-*.log에 보관한다.

### 작업 05 후속 수정: 팀 목록 404 분류

목록 조회의 404를 선택한 팀의 멤버십 오류와 구분했다. Mock 중앙에서 목록·상세·멤버 조회 각각의 404를 반환해 오류 코드와 복구 안내, 기존 선택·인증 상태 보존을 검증했다. 개별 team list는 RESOURCE_NOT_FOUND를 유지하고 setup 목록 404는 관리자 확인 안내를 반환한다. 명세의 HTTP 500도 서버 장애로 분류되는지 검사했다.

2026-11-02 Windows MSVC 기준 팀 선택 17건·기존 팀 CLI 5건·setup 흐름 10건·출력 5건으로 총 37건이 통과했다. fmt·workspace/all-targets Clippy `-D warnings`·변경 공백 검사는 통과했다. 전체 workspace 테스트와 실제 운영 서버 검증은 이번에 실행하지 않았다. 테스트 로그는 `rust/target/setup05-list-404-validation.log`에 보관한다.

### PR 리뷰 보완 검증

2026-11-02 Windows MSVC에서 로그인 브로커 충돌·입력 중 config 손상·비밀번호 EOF/빈 줄 구분·일반 권한 config 재사용·persist 전후 실패·입력 I/O 분류 회귀를 보강했다. 기존 init의 symlink 허용은 복원하고 setup의 상위 링크 거부 정책은 유지했다. 설정 필드 검증·스냅샷 읽기·잠금 판별의 중복도 정리했으며 broker_running은 준비 시점의 값임을 명시했다.

관련 회귀는 broker 라이브러리 56건, CLI 라이브러리 2건, setup 설정/인증/팀 선택 13/21/17건, 공통 흐름/출력 10/5건, 기존 setup/auth/team/Agent CLI 6/8/5/12건, 실행 중 브로커 보존 1건으로 **156 passed / 0 failed**다. 재실행한 항목은 중복 합산하지 않았다. 전체 fmt, workspace/all-targets Clippy `-D warnings`, 변경 공백 검사도 통과했다. 처음 샌드박스에서는 파일 persist가 차단돼 저장 후 실패 검사가 실행되지 않았고, 샌드박스 밖의 임시 프로필에서 통과를 확인했다. 기존 auth CLI의 HTTPS 진단이 가려지는 회귀를 발견해 문구를 유지하도록 보완한 뒤 인증 회귀를 재실행했다.

전체 workspace 테스트·운영 중앙/DB·실제 콘솔 숨김 입력·Unix 전용 symlink 테스트는 이번 검증에 포함하지 않았다.

### 코드 리뷰 후속 보완

2026-11-02 리뷰에서 config의 쓰기 권한 검사 누락을 보완했다. 읽기 공개는 유지하고 소유자·쓰기 권한만 검사하며, Windows는 기존 ACL 검사기에 읽기 허용 모드를 추가해 공유한다. 인증 중 config 읽기 실패·잠금 파일 오류·검증 필드를 각각 구분하고, init의 config 존재 확인은 링크를 따라가지 않으며 검사 오류를 전파한다. 입력 오류 match의 unreachable 분기와 auth 오류 위치 분기를 정리했다.

회귀: 인증 중 config 변경 4종(잘못된 URL·원격 ws·authTimeoutMs·삭제), 사용자 전환 중 broker.lock 오류, 다른 주체 쓰기 config 거부(Windows Everyone:M, Unix 0o664/0o646), 빈 stdin 안내, 끊긴 링크 init(Unix 전용)을 추가했다. Windows MSVC에서 fmt·Clippy `-D warnings`가 통과했고 setup 설정 14건·인증 22건·cli_auth 9건이 통과했다. 기존 테스트의 %TEMP%는 이 PC에서 다른 그룹에 Modify를 상속하므로 init 테스트는 상위 디렉터리를 비공개로 준비한다. Unix 전용 테스트는 이번에 실행하지 않았다.

기존 HEAD에서도 나타나던 병렬 테스트 두 건의 타이밍 가정을 수정했다. 제품 실행 로직은 변경하지 않았다.

- `central::tests::completed_storage_from_an_old_generation_cannot_ack_or_replace_new_identity`: 합성 서버가 하트비트 세 번 이후 PONG 응답을 중단했다. 병렬 디스크·ACL 처리가 500ms를 넘으면 저장 완료에 따른 세대 검증보다 하트비트 만료가 먼저 연결을 종료하고, 분리된 저장 작업이 끝나기 전에 파일을 검사해 0건을 읽을 수 있었다. 재현에서도 기대 1건/실제 0건으로 실패했다. 이제 하트비트 횟수와 함께 `has_pending_write()`로 실제 저장 접수를 확인한 뒤 세대를 교체하고, 저장 완료로 연결이 끝날 때까지 PONG을 계속 응답한다. 하트비트·소켓·runner timeout은 늘리지 않았다.
- `cli_creates_tasks_with_agent_bearer_and_reuses_request_key_after_restart`: 테스트가 영속 키 저장·중앙 REST 확인을 포함하는 작업 생성에도 `Runtime::client()`의 1초 health/control timeout을 사용했다. 병렬 묶음 반복 중 `/v1/tasks/create`에서 클라이언트 `TimedOut`을 재현했다. 요청별 timeout을 실제 서버의 `LOCAL_BUDGET`(8초)과 응답 수신 여유 1초로 설정해 서버가 정한 처리 결과를 기다리도록 했다. 재시도나 제품의 timeout 변경은 추가하지 않았고, 실패 시 HTTP 상태와 로컬 오류 본문을 함께 검사한다.

2026-11-02, Rust 1.99.1 `x86_64-pc-windows-msvc`에서 검증했다. 반복과 전체 실행 모두 기본 병렬 설정을 사용했으며 `--test-threads=1`이나 테스트 재시도를 사용하지 않았다.

| 검사 | 결과 |
| --- | --- |
| `cargo test --offline --locked -p devlink-broker --lib` | 56개씩 5회 연속 통과 |
| `cargo test --offline --locked -p devlink-cli --test cli_broker` | 35개씩 5회 연속 통과 |
| `cargo test --workspace` | 259 passed / 0 failed / 기존 opt-in 2 ignored |
| `cargo fmt --all -- --check` | 종료 0 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 종료 0 |
| `git diff --check` | 통과 |

로그는 `rust/target/parallel-stability/`의 `broker-lib-1.log`~`broker-lib-5.log`, `cli-broker-1.log`~`cli-broker-5.log`, `workspace.log`에 보관한다. 해당 폴더는 빌드 산출물이므로 Git에 추가하지 않는다. 기존 setup 변경 중 문서를 제외한 14개 파일은 작업 시작 시점과 SHA256이 동일하며, 문서는 이 절에 원인·해결·검증 결과만 반영했다.

#### Windows config의 Administrators 소유 허용

사용자가 선택한 정책에 따라 config의 Windows 소유자는 현재 사용자 SID 또는 BUILTIN\Administrators(S-1-5-32-544)를 허용한다. 관리자 셸에서 init으로 만든 config를 소유자 때문에 거부하던 문제를 해결한다. SYSTEM이나 다른 사용자/그룹 소유의 config는 계속 거부한다. 일반 다른 사용자의 쓰기·삭제·권한 변경 허용 및 부모 디렉터리의 같은 권한도 계속 거부한다. 세션·Agent·팀 선택·MCP 자격 파일은 현재 사용자 소유만 허용하며, Unix 정책도 유지한다.

Windows ACL 판정 함수를 실제 검사와 회귀 테스트에서 공유한다. 새 테스트는 실제 .NET FileSecurity 객체의 소유자를 현재 사용자·Administrators·SYSTEM·Everyone으로 바꿔 config와 비공개 파일의 허용/거부를 대조하고, 관리자 소유 config에서도 Everyone 쓰기 및 부모 쓰기를 거부하는지 검사한다. 관리자 권한 없이 메모리의 ACL 객체로 검사하므로 실제 파일 소유자나 사용자 환경의 권한을 변경하지 않는다. 기존 정책에서는 config 소유자 검사에서 실패하고 정책 변경 후 통과했다.

2026-11-02 Windows MSVC에서 새 정책 회귀와 기본 병렬 `cargo test --workspace`가 **260 passed / 0 failed / 기존 opt-in 2 ignored**로 통과했다. `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, 변경 공백 검사도 통과했다. 전체 실행 로그는 `rust/target/admin-owner-verification/workspace.log`에 보관한다. 이번 수정은 credentials.rs의 Windows ACL 정책·회귀와 이 문서에 한정하며 기존 setup 변경을 보존했다. 실제 관리자 셸에서의 init 수동 실행과 Unix 전용 테스트는 이번 검증에 포함하지 않았다.

#### P3 리뷰 수정: config와 시스템 파일 소유자 정책 분리

2026-11-03 리뷰에서 실제 config 검사에 실행 파일용 정책이 적용돼 SYSTEM·TrustedInstaller 소유까지 허용하던 불일치를 수정했다. config 소유자는 현재 사용자·Administrators만 허용한다. 실행 파일·상태 상위 디렉터리의 시스템 소유 허용은 별도 정책으로 구분한다.

테스트 전용으로 중복돼 있던 ACL 판정 함수를 실제 파일 검사와 공유하도록 옮겼다. 공통 함수에 이전 정책을 적용하면 SYSTEM 소유 config 사례가 실패하는 것을 확인한 뒤 수정했다. 메모리 ACL로 현재 사용자·Administrators·SYSTEM·TrustedInstaller·Everyone 소유를 검사하며, config의 공개 읽기 허용과 비신뢰 주체의 파일·부모 쓰기 거부도 검증한다. 실제 파일 소유자를 변경하지 않는다.

Windows MSVC에서 `cargo test --workspace --offline --locked`는 **293 passed / 0 failed / 기존 opt-in 2 ignored**다. 이후 보강한 TrustedInstaller 파일·부모 쓰기 검사도 해당 소유자 정책 테스트를 다시 실행해 통과했다. fmt·변경 공백 검사는 통과했다. 전체 Clippy `-D warnings`는 이번에 수정하지 않은 `mcp_config.rs:201`, `sessions.rs:558`의 기존 `collapsible_if` 진단으로 실패했으며, 해당 lint만 제외한 전체 타깃 검사는 통과했다. Unix와 실제 관리자 셸 수동 검증은 포함하지 않았다.

## 작업 06: 프로젝트 경로와 Agent 기본 정보

`--project <DIRECTORY>`는 기존 디렉터리를 받으며 `--project-path`도 같은 옵션이다. 생략하면 실행 시 현재 작업 폴더를 사용한다. 상대 경로·공백·한글을 지원하고 기존 Agent bootstrap과 같은 canonicalize 검사로 절대 경로를 확정한다. 없는 경로와 파일은 거부하며 프로젝트 폴더를 자동 생성하지 않는다.

validate는 설정 생성·로그인·팀 조회 전에 경로만 검사한다. 이때 확정한 절대 경로를 보관해 후속 단계에서 상대 경로나 현재 작업 폴더를 다시 해석하지 않는다. bind_project는 디렉터리 존재 여부를 재검사하고 control.lock 안에서 Agent 프로필을 읽어 프로젝트를 비교한 뒤, 저장된 이름을 반영한 메타데이터를 한 번 검증한다. 다른 디렉터리이거나 기존 디렉터리가 사라졌으면 `SETUP_PROJECT_BINDING_CONFLICT`/종료 6으로 중단한다. ready·needs_token·create_pending·vacant 모두 다른 프로젝트로 조용히 바꾸지 않는다. 별도 `--config` 프로필 또는 기존 `agent status` 확인 후 명시적 `agent use --project`로 복구하도록 안내한다.

프로젝트명·Agent명은 폴더명에서 각각 UTF-16 120·100 단위까지 기본 구성한다. 경계의 이모지를 자르지 않는다. `--project-name`·`--agent-name`·`--role-description`으로 명시할 수 있고, 이름은 공백만으로 구성할 수 없으며 역할은 255 단위 이하다. 명시값이 제한을 넘으면 자동 절단하지 않고 bind_project에서 `SETUP_INPUT_INVALID`/종료 2와 해당 필드 descriptor를 반환한다. 폴더명이 없는 파일시스템 루트는 두 이름을 명시하거나 같은 프로젝트의 기존 프로필에 저장된 이름이 있어야 하며, 이 검증을 validate로 당기면 기존 바인딩이 있는 사용자가 로그인 전에 막히므로 bind_project에서 수행한다. 같은 프로젝트의 기존 프로필에서는 생략한 이름·역할을 저장된 값으로 채우며, create_pending은 저장돼 있는 이름·프로젝트명만 재사용한다.

bind_project는 프로필·Agent/MCP 자격을 쓰거나 토큰을 발급/회전하지 않는다. 성공은 `outcome:not_applied`이며 기존 프로젝트를 재사용하면 `reused:true`다. 결과는 `SetupStages::project()`의 PreparedProject에 보관해 후속 도구·등록 단계에서 사용한다. 실제 프로필 저장은 후속 register_agent의 역할이다. 로컬 root를 가진 PreparedProject는 Serialize를 구현하지 않고, AgentMetadata는 이름·프로젝트 표시명·역할만 직렬화한다. 기존 create 요청도 이 공개 필드와 toolType만 중앙에 보내고 projectRoot는 로컬 비공개 프로필에만 저장한다.

없는 경로/파일은 `SETUP_PROJECT_PATH_INVALID`/종료 2, 프로젝트 바인딩 충돌은 종료 6, 손상된 프로필 읽기는 `SETUP_PROJECT_STORAGE_FAILED`/종료 5다. control.lock 경합은 기존 `SETUP_CONFIG_BUSY`/종료 6을 사용한다. 기존 단계가 손상된 Agent 파일을 먼저 발견하면 기존 설정/팀 오류를 유지한다. 복구 안내는 고정 문구이며 자격 원문은 출력하지 않는다. 작업 06 종료 당시에는 select_tool에서 종료 3이었으며 작업 07에서 도구 선택을 연결했다.

### 작업 06 검증 결과

2026-11-03 Windows MSVC에서 합성 중앙 서버·임시 로컬 프로필로 새 프로젝트 검사 10건과 기존 Agent CLI 12건, setup CLI 6건, 인증 22건, 설정 14건, 흐름 10건, 출력 5건, 팀 선택 17건이 통과했다. 최종 관련 회귀는 중복을 제외해 **96 passed / 0 failed**다. 프로젝트 테스트의 UTF-16 이모지 경계·신규 config 미생성 입력을 보강한 뒤 10건을 재실행했고, 기존 팀 테스트에도 연결된 프로젝트 단계를 통과하도록 같은 프로젝트 경로를 명시한 뒤 17건을 재실행했다. 이 재실행을 합계에 더하지 않는다.

한글·공백·상대/절대 경로·현재 폴더 기본값·없는 경로/파일 거부·설정 생성 전 거부·프로젝트 단계 전 디렉터리 삭제·이름과 역할의 UTF-16 경계·같은 프로젝트 이름 재사용·모든 기존 프로필 상태의 프로젝트 충돌·손상 파일과 자격 보존·control.lock 경합을 검사했다. 중앙의 실제 생성 요청은 합성 서버가 허용한 name/projectName/roleDescription/toolType 네 필드만 받는지 검사하고 로컬 projectRoot 저장과 구분했다. 프로필 재사용/거부 시 파일 바이트와 수정 시각도 보존됐다.

fmt·변경 공백 검사는 통과했다. 전체 workspace/all-targets Clippy `-D warnings`는 이번에 수정하지 않은 mcp_config.rs:201, sessions.rs:558의 기존 collapsible_if 두 건으로 실패했다. 해당 lint만 제외한 전체 타깃 검사와 최종 보강한 두 테스트 타깃의 Clippy는 통과했다. 최초 샌드박스에서는 임시 파일 교체가 차단됐으며 이후 허용된 샌드박스 밖 테스트에서 위 회귀를 확인했다. 전체 workspace 테스트·실제 운영 중앙·Unix 전용 경로·실제 대화형 콘솔 검증은 이번에 수행하지 않았다.

## 작업 07: Codex·Claude Code 탐지와 선택

`--tool codex|claude-code`로 선택하고 `--tool-path <FILE>`로 PATH 밖 설치를 지정할 수 있다. 경로 옵션은 도구 종류와 함께 사용한다. 명시 선택 → 기존 Agent 프로필의 도구 → 같은 중앙 origin/사용자/팀/프로젝트에 저장된 유효한 선택 순으로 사용한다. 선택이 없고 사용 가능한 도구가 하나면 그 도구를 선택한다. 여러 개면 ToolInput 어댑터에 요청하며 TTY가 없으면 `SETUP_TOOL_REQUIRED`/종료 2와 `tool.type/selection/detected_tool_list` descriptor 및 탐지 목록을 반환한다. 기존 Agent와 다른 도구를 명시하면 종료 6으로 중단한다. 기존 Agent의 도구가 없을 때도 다른 도구로 자동 전환하지 않는다. Agent에 묶이지 않은 저장 선택이 무효일 때는 현재 설치 목록에서 다시 선택한다.

탐색은 절대 PATH 항목과 사용자 `.local/bin`, `.cargo/bin`, Windows APPDATA/npm을 사용한다. Windows는 codex.exe/codex.cmd와 claude.exe/claude.cmd, Unix는 codex/claude를 찾는다. 상대/빈 PATH 항목을 현재 프로젝트에서 실행하지 않는다. 첫 PATH 명령이 실행 불가면 뒤의 같은 이름으로 조용히 우회하지 않는다. 명시한 실행 경로와 저장 경로를 우선 검사하며 canonical 실행 파일과 검증한 절대 경로 argv를 보관한다. Windows npm .cmd는 배치 문자열을 실행하지 않고 같은 설치의 알려진 패키지 진입점과 Node 실행 파일로 변환한다. Node 스크립트 인자는 일반 드라이브/UNC 경로로 전달하고 canonicalize 왕복으로 같은 파일인지 확인한다. 이전 빌드에서 저장한 verbatim 스크립트 인자도 재사용 전에 변환한다. 셸 명령 문자열이나 임의 저장 argv는 실행하지 않는다.

Windows pnpm·yarn 전역 설치 중 shim 옆의 `node_modules`에 패키지가 없는 배치는 현재 지원하지 않으며 `not_executable`로 표시된다. 같은 `.cmd`를 `--tool-path`로 지정해도 해결되지 않는다. 패키지의 실제 네이티브 `.exe`를 `--tool codex|claude-code --tool-path <FILE>`로 지정하거나 지원하는 npm/네이티브 설치를 사용한다. 임의 shim의 내용을 해석하거나 실행해 우회하지 않는다.

실행 가능 여부는 stdin을 닫고 임시 작업 폴더에서 버전 명령만 호출해 검사한다. 네이티브 도구에는 `--version` 하나, npm 도구에는 Node의 패키지 진입점과 `--version`만 전달한다. 탐지 과정은 프롬프트·exec·print 모드·로그인·설치·업데이트·모델 호출을 사용하지 않는다. 각 probe의 기본 timeout은 5초이며 stdout/stderr 각각 8 KiB를 넘으면 실패한다. timeout/출력 초과에서는 자식 프로세스를 종료·수거하고 원문 출력을 사용자 오류에 전달하지 않는다. Node preload 환경 NODE_OPTIONS/NODE_PATH는 probe에서 제외한다. 버전은 Codex의 codex-cli 접두사, Claude Code의 도구 접미사 및 숫자 버전 형식으로 확인한다.

탐지 DTO의 상태는 available/missing/not_executable/version_check_failed/timed_out/invalid_version이다. `tools` 목록은 probe한 도구의 상태·공식 설치 URL·available 도구의 installation(toolType/executable/version)·mcpAutomaticSupported를 제공한다. 도구를 지정하지 않으면 두 도구를 모두 probe하고, `--tool`·기존 Agent 바인딩·대화형 선택으로 도구가 정해지면 그 도구만 probe한다(Agent에 묶이지 않은 저장 선택은 먼저 단독 probe하고 실패할 때만 전체 탐지). npm shim은 Node 기동이 들어가고 probe당 최대 5초이므로 불필요한 probe를 피한다. 선택 요청의 waiting_input progress에도 같은 목록을 전달한다. 도구가 없거나 명시한 도구가 실행 불가면 `SETUP_TOOL_UNAVAILABLE`/종료 5와 설치 링크, 터미널 재실행, `setup --tool` 및 `--tool-path` 안내를 반환한다. 입력 취소는 종료 130, 입력 I/O 실패는 종료 1이며 인증·팀·프로젝트의 완료 상태를 보존한다. 선택 후 선택한 도구만 다시 probe하고 다른 도구의 이전 탐지 결과는 목록에 유지하므로, 입력 중 삭제된 실행 파일을 선택 완료로 처리하지 않으면서 probe를 반복하지 않는다.

선택은 `.devlink-runtime/selected-tool.json`에 schemaVersion/binding/selected로 비공개 원자 저장한다. binding은 origin/userId/teamId/projectRoot, selected는 toolType/executable{command,args}/version이다. 이 파일은 중앙 요청에 포함하지 않는다. 경로·버전을 실제 재검사한 뒤 내용이 같으면 파일 바이트·수정 시각을 보존하고 reused:true/not_applied를 반환한다. 첫 저장이나 선택 갱신은 applied다. 기존 Agent 선택 재사용은 새 캐시 파일을 저장해도 reused:true다. Agent/MCP 자격은 변경하지 않는다.

읽기와 저장은 account.lock → control.lock 순서로 보호한다. 버전 실행과 사용자 입력 중에는 잠금을 유지하지 않으며 저장 전에 로컬 세션·현재 서버·팀·프로젝트·Agent 도구·선택 스냅샷을 다시 대조한다. 다른 선택이 저장됐으면 `SETUP_TOOL_SELECTION_CONFLICT`/종료 6, 기존 Agent 도구 충돌은 `SETUP_TOOL_BINDING_CONFLICT`/종료 6, 잘못된 선택 파일은 `SETUP_TOOL_STORAGE_FAILED`/종료 5로 보존한다. 저장 결과가 불확실하면 unknown으로 멈춘다. 세션·서버 변경과 잠금 오류는 기존 setup 오류를 재사용한다.

`SetupStages::tool()`은 SelectedTool을 후속 모듈에 전달한다. 재실행할 때 command와 args를 그대로 재사용할 수 있고, 자동 설정 제공자는 `McpConfig::new(selected.verified_host()?)`로 만들 수 있다. 탐지 단계의 mcpAutomaticSupported도 이 VerifiedHost API의 결과다. 도구를 실행할 수 있는 것과 해당 버전의 자동 Host 설정을 지원하는 것은 별개다. 알 수 없는 버전도 available일 수 있지만 자동 설정 지원 여부는 제공자가 결정한다. Host 설정 위치·TOML/JSON 형식·지원 버전 판단은 [기존 MCP 설정 제공자](../../contracts/setup/v1/mcp-config-provider.md)에 위임한다. 이 단계는 Host 설정 파일을 고르거나 쓰지 않는다. Host 실행 정보와 MCP 서버용 Node/스크립트 실행 정보도 구분하며, 후자의 Node >=24·빌드 결과 검사는 후속 MCP 연동 작업이다.

버전 확인과 설치 안내 근거: [OpenAI 공식 CLI 문서](https://learn.chatgpt.com/docs/codex/cli), [Codex 버전 확인 예시](https://developers.openai.com/cookbook/examples/codex/using_goals_in_codex), [Claude Code 설치](https://code.claude.com/docs/en/setup), [Claude CLI 참조](https://code.claude.com/docs/en/cli-reference). 자동 설치를 실행하지 않는다.

### 작업 07 검증 결과

2026-11-03 Windows MSVC에서 신규 도구 회귀 13건, 기존 setup CLI 6건·설정 14건·인증 22건·팀 선택 17건·프로젝트 10건·흐름 10건·출력 5건·Agent CLI 12건이 통과했다. 중복을 제외한 관련 회귀는 **109 passed / 0 failed**다. 합성 도구는 Rust로 빌드한 실행 파일이며 받은 argv를 기록하고 허용된 버전 인자 외에는 실패한다. 설치 탐지는 AI 프롬프트나 작업 명령을 실행하지 않는다. 기존 setup 회귀는 명시한 합성 Codex 경로를 사용해 사용자 설치의 선택 상태에 의존하지 않도록 보완했다.

탐지 없음·두 도구 탐지·명시 한글/공백 경로·Node/npm shim·실행 불가/실패/잘못된 출력/출력 초과/timeout·PATH 우선순위·알 수 없는 버전의 제공자 판단·단일 선택 저장·다중 선택 입력/재개·저장 경로의 PATH 외 재사용·오래된 선택 교체·파일 바이트/수정 시각/비공개 권한 보존·기존 Agent 도구 우선/전환 거부·손상 argv 보존·계정/프로필 잠금·입력 취소/I/O 오류·입력 중 도구 삭제/서버/사용자/선택 변경을 검사했다. 자동 Host 설정 파일을 생성하지 않았으며 Agent 토큰을 발급/회전하지 않았다.

전체 타깃 check, fmt, 변경 공백 검사와 기존 collapsible_if lint만 제외한 workspace/all-targets Clippy `-D warnings`가 통과했다. lint 제외 없이 실행한 Clippy는 기존 mcp_config.rs:201, sessions.rs:558의 두 진단이 남아 있다. 전체 workspace 테스트, 운영 중앙/DB, 실제 AI 작업·모델 호출, 실제 콘솔 대화형 입력, Unix 전용 실행 권한 분기는 이번 검증에 포함하지 않았다. Agent 등록 및 실제 Host/MCP 연결은 후속 작업이다.

### 작업 07 리뷰: Windows npm 경로·테스트 timeout·전역 shim 범위

2026-11-03 Windows의 실제 Node 24.18.0에서 버전만 출력하는 ESM 파일을 검사했다. 일반 절대 경로는 종료 0, canonicalize의 verbatim 경로는 EISDIR/lstat C:와 종료 1이었다. 새 실제 Node 회귀도 수정 전에는 version_check_failed로 실패했다. 이전 npm 회귀는 Node 역할을 Rust 합성 실행 파일로 대체해 모듈 로딩 오류를 발견하지 못했다.

이제 파일 검사는 canonical 경로로 유지하고 Node 스크립트 argv만 일반 드라이브/UNC 경로로 변환한다. 변환 결과를 다시 canonicalize해 같은 대상인지 확인하므로 접두사 제거로 다른 파일을 실행하지 않는다. Node 실행 파일의 canonical 경로는 유지한다. 이전 선택 파일에 저장한 verbatim 스크립트 argv도 재사용 시 변환한다. 실제 Node·임시 ESM 패키지로 Codex/Claude 두 형식의 PATH 탐지·명시 .cmd 선택·PATH 밖 저장 경로 재사용과 한글/공백/&/#/% 경로가 수정 후 통과했다. 이 회귀는 Node 설치가 Rust 테스트의 필수 조건이 되지 않도록 opt-in ignored 테스트로 두고 다음 명령으로 별도 실행했다.

```powershell
cargo test --offline --locked -p devlink-cli --test setup_tools real_node_loads_npm_esm_entries_for_both_hosts_and_reuses_saved_paths -- --ignored
```

정상 탐지 검사에 공통 적용되던 800ms 제한을 제품 기본 5초와 통일했다. 의도적으로 30초 멈추는 합성 도구의 종료 검사만 별도 테스트에서 800ms를 사용한다. 짧은 제한과 실패 가능성은 코드에서 확인했지만, 이번 검토에서 기존 간헐 실패 자체를 재현한 것은 아니다. 제품 probe timeout은 변경하지 않았다.

shim 옆에 node_modules가 없는 전역 pnpm/yarn 배치는 코드 및 합성 배치 회귀에서 not_executable임을 확인했다. 이 구조의 자동 해석은 추가하지 않았으며, 실제 네이티브 실행 파일을 --tool-path로 지정하거나 지원하는 설치를 사용하라는 내용을 오류 안내와 문서에 추가했다. 같은 .cmd를 다시 지정하는 것으로 해결되지 않음을 명시했다. 실제 패키지 매니저 설치/변경과 AI 작업·모델 호출은 하지 않았다. 실제 네트워크 UNC 공유는 이번에 검증하지 않았다.

기본 병렬 `cargo test --workspace --offline --locked`는 **318 passed / 0 failed / 3 ignored**로 종료 0이다. ignored는 기존 opt-in 2건과 새 실제 Node 회귀 1건이며, 새 Node 회귀는 위 명령으로 따로 실행해 **1 passed**를 확인했다. 정상 탐지·전역 shim 제한·전용 timeout을 포함한 setup_tools는 전체 실행 안에서 15 passed / 1 ignored다. 재시도와 --test-threads=1은 사용하지 않았다. fmt·변경 공백 검사 및 기존 collapsible_if만 제외한 전체 타깃 Clippy가 통과했다. 전체 실행 로그는 빌드 산출물 `rust/target/setup07-review-workspace.log`에 보관한다. 이 결과는 이번 병렬 실행의 통과이며 모든 CI 부하에서 간헐 실패가 사라졌음을 보장하는 검증은 아니다.

## 작업 11: 브로커 시작·중지·상태 실행부 공통화

`devlink_broker::broker_control`로 기존 cli.rs의 실행 제어를 옮겼다. 모듈은 stdout/stderr에 출력하지 않으며 `start(configPath, cliExecutable)`, `stop(configPath)`, `status(configPath)`, `require_stopped(configPath)`를 제공한다. 개별 start/stop/status 명령은 이 모듈의 결과만 렌더링한다. 기존 성공 출력과 stopped/running 표시, 수신함/알림 진단을 유지한다. 실행 오류에는 typed Failure(kind/outcome/file)를 붙이고 setup은 허용 코드로 매핑한다. CLI에서 이미 제공하던 설정·토큰 충돌 진단은 Failure의 비직렬화 진단으로 유지한다. 원문 provider 오류나 private runtime/토큰을 setup 결과로 직렬화하지 않는다.

start는 control.lock을 유지하면서 broker.lock과 인증된 health를 확인한다. 이미 실행 중이면 service/API version/PID/instanceId와 control-token 인증을 통과한 health를 반환하며 reused:true다. 잠금이 잡혔는데 runtime/health를 확인할 수 없으면 실패하며 새 프로세스를 만들거나 기존 PID를 종료하지 않는다. local::running의 기존 broker.lock 기반 판정과 stale runtime 정리도 유지한다. PID 파일만 있으면 running으로 간주하지 않는다.

새 프로세스는 지정한 CLI 실행 파일에 --config <FILE> broker만 전달한다. stdin/stdout/stderr는 null이며 Windows의 DETACHED_PROCESS/CREATE_NEW_PROCESS_GROUP를 유지한다. 부모 표준 핸들의 HANDLE_FLAG_INHERIT는 이 spawn 동안만 해제하고 spawn 직후 원래 플래그로 복원하므로, 오래 실행되는 setup이 이후 단계에서 stdio를 상속하는 Host 자식을 띄워도 영향을 받지 않는다. Unix는 process_group(0)을 유지한다. 라이브러리 호출자는 자신의 current_exe가 테스트/다른 앱일 수 있으므로 실제 devlink 실행 파일을 넘긴다. CLI는 current_exe를 사용하고 setup은 기본값 외에 with_broker_executable로 주입할 수 있다. windows-sys는 이미 고정된 workspace 버전을 브로커 crate에도 연결했으며 외부 패키지 버전은 바꾸지 않았다.

기동 제한은 기존 8초 + credentials::startup_validation_allowance() + agent_profile::startup_allowance(configPath)다. 50ms 간격으로 직접 만든 Child의 종료를 확인하고, runtime 로드·해당 Child PID 일치·인증 health 확인을 통과한 뒤 broker.lock을 확인한다. 시작 대기 중 timeout/오류/취소가 발생하면 RAII 가드가 바로 그 Child 핸들만 kill/wait한다. 성공 시 가드를 해제하고 독립 브로커를 남긴다. 다른 runtime의 PID로 프로세스를 찾아 종료하지 않는다. 기동 실패 전에 자격 파일 등이 발행됐을 수 있으므로 spawn 이후 실패는 outcome:unknown으로 반환한다.

stop은 control.lock 안에서 health를 인증한 뒤 같은 control token으로 shutdown 요청을 보낸다. health 검증과 요청은 기존 Runtime::stop과 같은 순서이며 중복 health 요청을 추가하지 않는다. 요청 뒤 최대 8초 동안 broker.lock 해제를 기다리고 timeout이나 장애 때도 발견한 PID를 강제 종료하지 않는다. stopped는 AlreadyStopped, 정상 종료는 Stopped로 구분한다. 로컬 인스턴스 검증과 중앙 WS/업무 신원의 일치는 구분한다. start 성공 자체가 중앙 Connected나 전체 setupReady를 의미하지 않는다.

프로필 변경·중앙 Agent 토큰 발급·계정 전환 전의 기존 agent_profile::stopped는 이제 require_stopped를 공유한다. control.lock과 broker.lock을 모두 잡아 변경이 끝날 때까지 유지하며 runtime 발행 전 기동 중인 브로커도 차단한다. 기존 AuthFault 분류와 복구 안내를 보존한다. 실행 중인 브로커를 setup이 자동 중지하지 않으며 devlink stop을 명시적으로 실행하도록 안내한다. 사용자가 정상 중지한 뒤 변경을 재시도하면 같은 잠금 검사를 다시 수행한다. 기존 MCP 자격 회전의 중지 요구 정책도 유지한다.

setup StartBroker 어댑터는 같은 start를 호출하고 결과를 SetupStages::broker()에 보관한다. 신규 기동은 applied/reused:false, 재사용은 not_applied/reused:true다. 공통 실행부의 시작/신원 검증 실패는 SETUP_BROKER_START_FAILED/종료 5·inspect_broker_then_retry로, 설정/잠금/자격 실패는 기존 setup 코드로 매핑한다. broker.lock은 잡혀 있지만 runtime.json이 아직 발행되지 않은 기동 중 브로커(BrokerStarting)는 잠시 뒤 다시 시도하면 풀리므로 SETUP_UNSAFE_FILE 대신 재시도 가능한 SETUP_BROKER_BUSY/종료 6으로 보고한다. runtime.json이 있으나 읽을 수 없는 경우만 Storage/SETUP_UNSAFE_FILE이다. 작업 08~10의 Agent 등록 흐름을 대신 구현하거나 건너뛰지는 않는다. 현재 일반 setup은 register_agent에서 종료 3이며 어댑터 통합 검사는 이미 등록된 합성 프로필에서 StartBroker를 호출해 검증한다.

### 작업 11 검증 결과

2026-11-03 Windows MSVC에서 신규 회귀는 공통 실행부 7건·setup/CLI 교차 제어 3건·캡처 파이프 EOF 1건으로 11건이 통과했다. 실제 기동·재사용·상태·명시 종료·중지 전 토큰 발급 차단/중지 후 발급·잘못된 제어 토큰 보존·포트 충돌·조기 종료/timeout/취소의 소유 자식 수거·다른 runtime PID 보존·서비스/API 버전/PID/instance 불일치 차단을 포함한다.

최종 `cargo test --target-dir target/setup11-verify --workspace --offline --locked --no-fail-fast -j 2`는 **330 passed / 0 failed / 기존 opt-in 3 ignored**로 종료 0이다. 새 11건은 이 합계에 포함한다. 기존 CLI 브로커 36건에는 동시 시작·느린 정상 ACL 기동·MCP 토큰과 중앙 WS 회귀가 포함되며, Agent CLI 15건·setup 설정/인증/팀/프로젝트/도구 회귀도 통과했다. -j 2는 컴파일 작업 수만 제한하며 테스트는 기본 병렬 설정이었다. --test-threads=1과 실패 테스트 자동 재시도는 사용하지 않았다.

첫 전체 실행에서 새 typed 오류가 기존 CLI의 토큰 충돌 진단을 가리는 문제를 발견해 원래 진단을 보존하도록 수정했고, 해당 기존 회귀를 단독 확인한 뒤 최종 전체 실행에서 재검증했다. 공용 target의 테스트 실행 파일 잠금으로 발생한 LNK1104는 실행 중인 다른 프로세스를 종료하지 않고 별도 target을 사용해 해결했다. 로그는 빌드 산출물 `rust/target/setup11-workspace.log`, `rust/target/setup11-workspace-final.log`, 최종 `rust/target/setup11-isolated-workspace.log`에 보관한다.

전체 fmt·변경 공백 검사가 통과했으며 기존 collapsible_if lint만 제외한 workspace/all-targets Clippy도 통과했다. 동시 수정 중이던 task_creation.rs의 포맷 차이는 해당 변경이 정리된 뒤 전체 검사를 다시 확인했으며, 이번 작업에서 별도 작업의 MCP/중앙/작업 처리 코드를 수정하거나 되돌리지 않았다. 테스트는 임시 프로필과 합성 중앙/자식 프로세스 기준이며 운영 중앙·실제 모델·Unix 전용 분기는 실행하지 않았다. 일반 setup의 register_agent 미연결 범위는 유지한다.

### 작업 11 리뷰: 기동 대기와 broker.lock 경합

2026-11-03 리뷰에서 부모의 `wait_for_start`가 runtime 발행 전에 `local::running`을 호출하면 자식이 필요한 broker.lock을 잠깐 획득할 수 있음을 확인했다. runtime 로드·자식 PID 일치·인증 health를 먼저 확인하고 마지막에 `local::running`을 호출하도록 순서를 바꿨다. runtime을 발행하지 않는 자식을 기다리는 동안 broker.lock 파일 자체를 만들지 않는 회귀도 추가했다. 마지막 락 검사의 오류는 PID 일치와 인증 health까지 확인된 브로커를 종료하지 않도록 다음 폴링으로 넘기고, 기동 제한 시간까지 계속 실패하면 StartupTimedOut 대신 마지막 오류를 `Storage / Unknown`과 broker.lock 경로로 반환한다(PR #41 리뷰 반영). Windows 회귀는 별도 심볼릭 링크 권한 없이 생성 가능한 junction으로 reparse point 거부와 대상 파일 보존을 확인하며, Unix 파일 symlink 분기는 이번 환경에서 실행하지 않았다.

별도 status 프로브도 같은 락을 잠깐 소유할 수 있어 자식의 최초 락 획득에만 10ms 간격·200ms 한도의 재시도를 추가했다. 재시도 대상은 `try_lock == None`인 경합뿐이며 경로 안전 검사/I/O 오류는 즉시 실패한다. 계속 점유한 경우 기존 `Another DevLink operation is running` 메시지를 유지한다. 가상 시계 테스트로 일시 경합 후 성공·지속 경합의 제한 시간·안전하지 않은 경로의 즉시 실패를 검사하고, 실제 자식 프로세스에서도 짧은 점유 후 기동과 지속 점유 시 실패를 확인했다. 새 설정 옵션이나 일반 재시도 계층은 추가하지 않았다.

제안 1·2·4를 적용했다. 제안 3의 알림/수신함 삭제 및 MCP 토큰 회전 가드 통일은 이번 기동 실패 해결에 필요하지 않은 별도 리팩터링이므로 제외했다. 해당 경로의 기존 안내 메시지와 중지 요구 정책은 회귀 테스트로 확인했다.

같은 `target/setup11-verify`에서 `cargo test --offline --locked -p devlink-broker --lib`는 **101 passed**, `cargo test --offline --locked -p devlink-cli --test cli_broker`는 **37 passed**로 총 **138 passed / 0 failed**다. 기존 `broker_control::tests` 7건과 새 Storage 회귀 1건, `detached_lifecycle_duplicate_and_local_authentication`, `simultaneous_start_cannot_spawn_two_brokers`, 캡처 파이프 EOF, 수신함 삭제 및 MCP 토큰 회전 회귀를 포함한다. 마지막 파일 상태에서 공통 실행부 8건을 다시 실행해 통과했다. 전체 workspace 테스트를 이번 리뷰에서 다시 실행하지는 않았다.

빌드된 `target/setup11-verify/debug/devlink.exe`를 출력 캡처 상태에서 한글·공백이 있는 별도 임시 프로필로 **start/stop 100회** 실행했다. **100/100 성공, 명령 실패 0건, `Broker exited during startup` 0건**이며 약 128초가 걸렸다. 합성 토큰과 loopback 중앙 주소만 사용했고 마지막 정상 stop도 성공했다. 재현 스크립트는 빌드 산출물 `target/broker-start-stop-stress.py`, 결과는 `target/broker stress 한글 1793681791792299200/result.json`에 보관한다. 이 반복 검증은 모든 부하에서 경합이 없음을 보장하는 증명은 아니다.

수정한 Rust 파일의 rustfmt·변경 공백 검사와 broker/CLI의 all-targets Clippy가 통과했다. Clippy는 기존 `collapsible_if` 진단만 제외했다. 운영 중앙·실제 AI 작업·Unix 실행 검증은 포함하지 않았다.

### PR #41 리뷰 반영

2026-11-03 PR #41 리뷰의 인라인 지적 7건을 반영했다. (1) broker.lock은 잡혀 있지만 runtime.json이 없는 기동 중 브로커를 `BrokerStarting`으로 구분해 setup에서 SETUP_UNSAFE_FILE 대신 재시도 가능한 SETUP_BROKER_BUSY로 보고한다. CLI start/status/stop도 "Broker is starting; wait a moment and retry"를 출력한다. (2) `wait_for_start`는 health까지 확인한 뒤의 `local::running` 오류로 자식을 종료하지 않고 제한 시간까지 재시도하며, 계속 실패하면 마지막 오류를 Storage/Unknown으로 반환한다. (3) validate는 프로젝트 경로만 확정하고 표시명·Agent명·역할 검증은 bind_project로 옮겨, 폴더명이 없는 루트에서도 저장된 프로필 이름으로 진행할 수 있다. (4) Windows 표준 핸들 상속 해제는 spawn 동안만 적용하고 직후 원래 플래그로 복원한다. (5) 도구 probe는 지정·바인딩·선택된 도구만 수행하고 대화형 재진입 때 선택한 도구만 다시 probe한다. (6) SetupStages의 단계별 상태를 auth/teams/project/tools/broker 하위 struct로 묶어 입력 어댑터 교체가 필드 9개만 옮긴다. (7) resultText 크기 검사와 함께 지적된 중앙 계약 변경(메시지 한도 64→256 KiB, REJECTED 전환, 완료 결과 크기 검사)은 브로커 제어와 무관하므로 이 브랜치에서 작업 전 상태로 되돌렸다. 중앙 서버 상한과 함께 별도 작업으로 맞춘다.

Windows MSVC에서 `devlink-broker --lib` **102 passed**(기동 중 분류·제한 시간 후 Storage 회귀 포함), broker 통합 task_operations 22·task_creation 7, CLI setup 8개 스위트(프로젝트 루트/bind_project 이름 검증·단독 probe·선택 재진입 probe 회귀 포함) 102 passed / 1 ignored, cli_agents 15·cli_broker 37이 모두 통과했다. workspace all-targets Clippy `-D warnings`(기존 collapsible_if 제외)와 fmt가 통과했다. 로그는 빌드 산출물 `rust/target/pr41-review-fix.log`에 보관한다. 전체 workspace 테스트와 Unix 분기는 이번에 다시 실행하지 않았다.

## 작업 08: Agent 생성·재사용 및 pending 복구 연결

이 절과 08 검증 수치는 당시 구현 기록이다. 토큰 통합 이후의 현재 setup 동작은 아래 작업 09를 따른다.

2026-11-03 (Asia/Seoul). `agent_profile::registration`은 기존 프로필·private 저장·사용자 세션·중앙 Agent API를 재사용하는 내부 provider다. `inspect`는 Agent를 만들거나 바인딩/발급하지 않는다. `ensure`는 명시적 Create/Use(ID), 또는 저장 프로필 재확인을 처리한다. 공통 실행기·provider는 터미널 입출력을 직접 수행하지 않고 AgentInput 어댑터를 사용한다.

- 새 프로필은 `--create-agent` 또는 대화형 create 입력이 있어야 생성한다. 이름·프로젝트명·역할·도구만 중앙 DTO에 넣고 로컬 경로·setup runId·추측한 등록키는 보내지 않는다.
- 기존 프로필은 서버 origin·확인한 사용자·팀·canonical 프로젝트·도구와 중앙 소유권/메타데이터를 대조한다. 정상 재실행과 동일한 create 요청은 기존 ID를 재사용하며 토큰을 발급하거나 파일을 다시 쓰지 않는다. ready는 기존에 저장된 토큰이 있는 상태이며 이번 08 단계가 토큰의 현재 유효성을 검증했다는 뜻은 아니다. 실제 기동에서는 기존 prepare_broker 검증을 거친다.
- `--agent-id <ID>`는 중앙에서 본인 소유·선택 팀·도구를 확인한 뒤 명시적으로 바인딩한다. 이름 일치로 자동 선택하지 않는다. `--create-agent`와 동시에 지정할 수 없으며 agent-name/project-name/role-description으로 중앙 메타데이터를 바꾸지 않는다.
- 생성 POST 전 CSRF를 준비하고 create_pending을 내구 저장한다. 응답 유실·5xx·잘못된 성공 DTO는 creation_unknown/outcome:unknown이며 pending을 유지한다. 재실행해도 생성 POST를 반복하지 않는다. 확정된 4xx 거부만 vacant로 저장하며 새 생성은 별도 명시 요청이 필요하다.
- 성공한 ID를 확인하면 needs_token을 저장하고 알려진 ID·outcome:applied와 SETUP_TOKEN_REQUIRED를 반환한다. 후속 09 또는 기존 명시적 `devlink agent token`으로 토큰을 준비할 수 있다. 발급·회전은 08에서 호출하지 않는다.
- 생성 결과 불명은 `devlink agent list`로 실제 본인 ID를 확인한 뒤 `setup --agent-id <ID>`로 복구한다. 등록키 기반 자동 복구 API는 아직 없으므로 제공한 것으로 간주하지 않는다. 자동 reset-pending/중복 위험 우회도 추가하지 않았다.
- 등록 입력/네트워크 대기 후 현재 바인딩을 다시 검사한다. account → control → broker → owner 순서로 잠그고, 실행/시작 중인 브로커의 변경은 거부한다. 손상 프로필을 명시적 use로 조용히 덮어쓰지 않는다. API 원문·토큰·쿠키를 공통 오류로 복사하지 않는다.

Agent 소유권/팀 불일치와 HTTP 403은 계약의 SETUP_AGENT_FORBIDDEN(4), requiredInputs:agent.id, select_owned_agent로 통일한다. 다른 새 오류는 SETUP_AGENT_BINDING_CONFLICT(6), SETUP_AGENT_REJECTED(4), SETUP_AGENT_STORAGE_FAILED(5)다. 상태·알려진 ID·적용 여부를 보존하며 복구는 명시적 사용자 동작으로 안내한다. 이 작업의 검증은 합성 중앙/실제 CLI 기반 회귀이며 실제 운영 중앙 API·배포본·Host 전체 설정 완료는 별도 검증이다.


### 작업 08 검증 결과

2026-11-03 Windows MSVC, 합성 중앙·실제 CLI 자식 프로세스·임시 비공개 프로필 기준이다. 기본 병렬 설정으로 실행했고 중복 재실행을 합산하지 않았다.

| 검사 | 결과 |
| --- | --- |
| 새 Agent 등록/복구/선택/잠금/CLI 계약 | 13 passed |
| 기존 Agent CLI / setup CLI | 15 / 6 passed |
| setup 인증 / 팀 / 프로젝트 | 22 / 17 / 10 passed |
| 공통 흐름 / 출력 / 도구 탐지 | 10 / 5 / 15 passed, 도구의 기존 opt-in 1 ignored |
| 관련 회귀 합계 | 113 passed / 0 failed / 1 existing opt-in ignored |
| workspace/all-targets Clippy `-D warnings` / 전체 rustfmt / 변경 공백 | 통과 |
| README·setup 문서의 로컬 링크 | 정상 |

등록 검증에는 명시적 생성/ID 선택, 토큰 발급 0건, needs_token·pending 내구 상태, 응답 유실 후 생성 POST 1회 유지, 확정 거부와 CSRF 선행 실패 구분, 본인 ID 복구, 타 사용자/팀/도구 거부, 토큰/프로필 원문 보존, 손상 파일 보존, 계정·제어 잠금 경합, 실제 생성 응답 대기 중 broker.lock 유지, 입력 취소, 단일 JSONL 최종 결과와 Java Long 최댓값의 문자열 보존을 포함한다.

초기 실행에서 손상 프로필의 오류 분류와 Windows 일반/정규화 경로의 동일성 비교를 보완했다. 앞 단계 테스트는 기존의 미구현 종료 3 대신 등록 선택 입력 부족 종료 2를 기대하도록 갱신했다. 최종 브로커 잠금 보완 후 등록 13건과 setup CLI 6건을 다시 실행해 통과했다. 로그는 `rust/target/setup08-regression-final.log`(초기 관련 묶음), `setup08-final-deltas.log`(프로젝트·팀 보완 후), `setup08-lock-final.log`(최종 등록·CLI), `setup08-clippy-final.log`에 보관한다. 각 로그의 코드 상태·중간 실패·최종 재검증을 구분한다.

전체 workspace 테스트, 실제 운영 중앙/DB, 실제 대화형 콘솔 수동 입력, Unix, 배포본·실제 Host 전체 설정은 이번 검증에 포함하지 않았다. 등록키 자동 복구는 미제공 API를 가정하지 않고 pending/본인 ID 복구로 남긴다.


### 작업 08 리뷰 보완

타인 소유/다른 팀 Agent의 정상 DTO와 HTTP 403은 SETUP_AGENT_FORBIDDEN/종료 4로 통일한다. 소유권 확인 실패로 Agent를 생성하거나 프로필을 바꾸지 않는다.

웹 등에서 같은 Agent의 이름·프로젝트 표시명·역할이 변경되면 일반 재실행/inspect/create는 기존 메타데이터를 보존하고 충돌을 반환한다. 브로커를 중지한 뒤 `setup --agent-id <동일 ID>`로 명시적으로 선택하면 중앙 소유권·팀·도구·프로젝트 경로를 다시 확인하고 최신 표시 메타데이터를 로컬 프로필과 후속 단계 컨텍스트에 반영한다. 기존 ready 토큰과 MCP 자격은 그대로 보존하며 생성·토큰 발급 API를 호출하지 않는다. 같은 메타데이터의 반복 선택은 다시 저장하지 않는다.

08 리뷰 당시에는 ready 토큰을 검사하지 않아 무효 토큰이 start_broker의 SETUP_BROKER_START_FAILED/종료 5로 끝났다. 아래 09 구현에서 register_agent가 먼저 토큰을 검증하고, 확인된 인증 거부는 SETUP_TOKEN_REQUIRED/종료 3과 명시적 발급 안내로 처리하도록 해소했다. 검증 이후 기동 사이의 서버/토큰 변경 경합과 실제 기동 실패는 별도의 브로커 오류로 남는다.


2026-11-03 리뷰 보완 검증: 수정 전 새 소유권·메타데이터 복구 회귀 2건이 모두 실패해 지적을 재현했다. 수정 후 등록 15건·도구 17건·setup CLI 6건·공통 흐름 10건·출력 5건으로 **53 passed / 0 failed / 기존 opt-in 1 ignored**다. 기본 병렬 설정으로 실행했으며 중복 재실행을 합산하지 않았다. workspace/all-targets Clippy `-D warnings`, rustfmt, 변경 공백 검사도 통과했다. 로그는 `rust/target/setup08-review-before.log`, `setup08-review-regression.log`, `setup08-review-clippy.log`에 보관한다. 운영 서버·전체 workspace 테스트는 이번에 다시 실행하지 않았다.


## 작업 09: 토큰 검증·재사용·명시적 발급 통합

2026-11-03 (Asia/Seoul). `agent_profile::tokens`가 기존 private 프로필·사용자 인증/CSRF·Agent 발급 API와 Agent Bearer `/api/v1/agents/me`를 재사용한다. 기존 개별 Agent 명령·브로커와 setup은 같은 제한 시간·응답 크기 제한·프록시/리다이렉트/자동 재시도 금지 정책의 typed 검증기를 공유한다. 토큰은 내부에서만 취급하고 공개 결과에는 ID·검증/재사용/발급 여부·outcome·안전한 메타데이터만 담는다.

- 정상 저장 토큰은 현재 user/team/agent/tool 신원을 검증한 후 바이트·수정 시각·MCP 자격을 보존한다. `--issue-token`이 있어도 정상 토큰을 회전하지 않는다. 강제 회전은 기존 `devlink agent token`의 명시적 별도 명령이다.
- 이번 실행에서 새로 생성된 Agent만 최초 발급 1회를 허용한다. 기존 needs_token 또는 Agent 인증 401/403에는 `--issue-token`이 필요하며, 없으면 register_agent에서 SETUP_TOKEN_REQUIRED/종료 3과 agent.issueToken descriptor를 반환한다. `--create-agent --issue-token`은 입력 충돌이다.
- 네트워크 장애·5xx·잘못된 JSON·리다이렉트·신원 모순은 토큰 만료로 단정하지 않는다. 각각 CENTRAL_UNAVAILABLE/PROTOCOL_ERROR로 반환하고 명시적 허용이 있어도 자동 발급하지 않는다. 사용자 세션 401과 Agent Bearer 거부, 로컬 MCP 자격 거부를 섞지 않는다.
- 발급 전 같은 origin/user/team/project/tool 및 중앙 소유권을 다시 확인한다. account → control → broker → owner 잠금을 유지하며 실행 중 브로커가 있으면 SETUP_BROKER_BUSY로 중지를 안내한다. setup은 발급을 위해 다른 프로세스를 자동 중지하지 않는다.
- CSRF 준비 후 POST 전에 needs_token을 내구 저장해 옛 토큰을 제거한다. 1회 발급 응답의 ID와 토큰의 실신원을 검증하고 MCP 자격 회전/프로필 저장까지 성공해야 ready를 게시한다.
- 발급 응답 유실/5xx·잘못된 발급 ID·새 토큰 검증 실패·발급 후 저장 결과 불명은 알려진 Agent ID와 SETUP_TOKEN_REQUIRED/outcome:unknown을 보존한다. needs_token에서 새 Agent를 생성하거나 옛 토큰을 복원하지 않는다. 단계의 발급 허용은 1회 소비되므로 같은 실행기를 다시 호출해도 불명한 요청을 자동 반복하지 않는다. 복구 후 새 `setup --issue-token` 요청만 새 발급을 허용한다.
- 정상 토큰 준비 후 기존 start_broker 어댑터로 진행한다. MCP 통합이 아직 없으므로 전체 setup은 configure_mcp의 SETUP_STEP_UNAVAILABLE/종료 3으로 멈출 수 있다. 이 경우 등록·토큰·브로커 준비는 이미 적용된 상태이며 configure_mcp 오류에 알려진 ID를 유지한다.

09는 기존 API 기반 구현이다. 등록키 기반 Agent 생성 자동 복구 API는 추가하지 않았으며, 토큰 발급 실패를 새 Agent 생성으로 우회하지 않는다. 실제 운영 중앙/DB·배포본·Host 전체 설정 검증은 별도다.


### 작업 09 검증 결과

2026-11-03 Windows MSVC, 합성 중앙 서버·실제 CLI 자식 프로세스·임시 비공개 프로필 기준이다. 기본 병렬 설정으로 실행했다.

| 검사 | 결과 |
| --- | --- |
| 새 토큰 검증·발급·장애/복구·CLI 연결 | 12 passed (초기 10건 + 권한 경계 2건, 중복 없음) |
| 기존 Agent 명령 / setup CLI / 등록·복구 | 15 / 6 / 15 passed |
| 도구 탐지 / 공통 실행 흐름 / 출력 | 17 / 10 / 5 passed, 기존 opt-in 1 ignored |
| 관련 회귀 합계 | 80 passed / 0 failed / 1 existing opt-in ignored |
| workspace/all-targets Clippy `-D warnings` / 전체 rustfmt / 변경 공백 | 통과 |

새 검증은 정상 토큰·파일 수정 시각·MCP 자격 보존, 최초/명시 발급, 무효 토큰의 기동 전 차단, 실행 중 브로커 변경 거부, CSRF 선행 실패와 확정 권한 거부 구분, 발급 전 needs_token 내구 저장 및 계정/제어/브로커 잠금 유지, 응답 유실 시 단계 허용 1회 소비, 신원/발급 ID 모순·유효하지 않은 토큰·네트워크/JSON/리다이렉트 오류, 발급 후 private 저장 실패와 ID 보존을 포함한다. 실패 후 새 Agent를 만들지 않으며 공개 결과·CLI 출력에 토큰이 없는지 확인했다.

실제 CLI 통합은 신규 생성 → 최초 토큰 발급/검증 → 브로커 시작 → configure_mcp 미연결 안내까지 통과했다. 같은 프로필 재실행 및 정상 토큰에 --issue-token을 추가한 실행에서 생성/발급 횟수가 각각 1회를 유지하고 기존 브로커를 재사용했으며, 소유 브로커를 정상 중지했다. 이 검증은 Host가 실제 MCP 설정을 소비했다는 증거는 아니다.

로그: `rust/target/setup09-regression.log`, `setup09-permission.log`, `setup09-adapters.log`, `setup09-clippy-final.log`. 전체 workspace 테스트·실제 운영 중앙/DB·Unix·배포본·실제 Host 전체 설정은 이번에 실행하지 않았다.
