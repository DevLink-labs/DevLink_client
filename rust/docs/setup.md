# setup 명령과 공통 실행 흐름

작업 02·03·04·05. 2026-11-02 (Asia/Seoul). [01 호출·단계·오류 규격](../../contracts/setup/v1/README.md)을 기준으로 CLI 진입점·공통 흐름·설정 초기화·사용자 로그인·팀 선택을 구현했다.

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
```

전역 `--config`는 명령 앞/뒤 모두 기존 파서를 통해 전달되며, 공백·한글 경로도 지원한다. 생략 시 기존 `$HOME/.devlink/config.json`을 사용한다.

현재 구현은 **명령 진입점·공통 실행 흐름·출력 어댑터·설정 준비·사용자 세션 확인/로그인·팀 조회/선택/재사용**이다. 프로젝트·도구·Agent 선택과 등록/기동/MCP 설정/상태 조회 연결은 후속 작업이다. `--input`, `--non-interactive`, `setup status`는 아직 제공하지 않는다.

기본 실행기는 validate에서 설정을 준비한다. 파일이 없으면 별도 init 없이 기본값을 비공개 권한으로 생성하고 `succeeded/applied/reused:false`로 기록한다. 유효한 기존 설정 파일은 보존하고 `succeeded/not_applied/reused:true`로 기록한다. authenticate에서 정상 세션을 서버로 확인해 재사용하거나 필요한 경우만 로그인한다. select_team에서 현재 가입 팀을 검증하고 선택한다. 완료되면 아직 미연결인 bind_project에서 `SETUP_STEP_UNAVAILABLE`과 종료 코드 3을 반환한다. TTY도 로그인 입력도 없는 미로그인 실행은 authenticate에서 종료 4와 필요한 입력 descriptor를 반환한다. 앞서 완료한 설정 준비·확인된 세션/팀 변경은 유지한다. 기존 개별 명령도 계속 사용할 수 있다.

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
| `devlink-cli/src/setup/terminal.rs` | clap 옵션·human/JSON/JSONL 렌더링·프로세스 종료 코드 전달 |
| `devlink-cli/src/terminal_input.rs` | 기존 auth/team 명령과 setup이 공유하는 숨김/전용 stdin 입력 |
| `devlink-broker/src/user_auth.rs` | 기존 로그인·CSRF·세션 저장과 typed session check·장애 분류 |
| `devlink-broker/src/teams.rs` | 현재 사용자·목록·상세·멤버십 검증과 선택 저장 |
| `devlink-broker/src/agent_profile.rs` | 기존/미확정 Agent 바인딩의 사용자·서버·팀 충돌 확인 |
| `devlink-broker/src/setup_config.rs` | 설정 경로·형식·값·자격 파일 안전성·브로커 잠금 검사 |
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

설정은 기존 private snapshot reader의 사용자 소유권·권한·일반 파일·16 KiB 상한 검사를 거쳐 읽는다. JSON/스키마·중복 키·알 수 없는 필드·값 형식을 검사하고 기존 Config 검증을 재사용한다. 사용자 인증 origin 검증도 재사용하므로 원격 중앙은 wss, ws는 localhost/loopback만 허용한다. agent·포트·재연결·인증 제한시간도 검증한다. 기존 3필드 설정은 기본값을 메모리에 보충하며 파일을 재작성하지 않는다.

새 부모 디렉터리만 비공개 권한으로 준비한다. 기존 디렉터리/파일의 불안전한 권한은 자동 수정하지 않는다. 새 설정은 기존 init이 사용하는 기본값·같은 디렉터리 임시 파일·flush 경로를 공유하고, 설정 생성은 persist_noclobber로 다른 생성 결과를 덮어쓰지 않는다. setup의 임시 파일에는 기록 전 비공개 권한을 적용한다. 기존 init도 같은 생성 함수를 사용하며 이미 존재하는 설정을 보존한다.

control.lock을 확보한 뒤 broker.lock을 확인한다. 다른 제어 작업이 있으면 CONFIG_BUSY다. broker.lock이 점유됐으면 실행 중/시작 중으로 취급한다. 기존 정상 설정은 읽기 검증만 허용하며 누락 설정 생성은 BROKER_BUSY로 중단한다. health/PID만으로 실행 여부를 추정하거나 브로커를 중지하지 않는다. 중지 상태에서는 broker.lock도 유지해 초기화 중 직접 브로커 기동과 충돌하지 않게 한다. 상태 조회 도중 stale runtime 파일을 삭제하는 기존 local::running은 이 단계에서 호출하지 않는다.

기존 broker/user-session/selected-team/agent-profile 파일이 있으면 private reader로 크기·권한·JSON 안전성을 확인하고 MCP 파일은 기존 credentials::load로 스키마도 검증한다. 자격을 발급·회전·삭제하지 않고 원문을 출력하지 않는다. 사용자·팀·Agent의 권한/세션 유효성 확인은 후속 단계 책임이며 이 저장소 검사가 중앙 인증 성공을 뜻하지 않는다.

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

초기화 중 저장 오류는 unknown으로 보수적으로 기록하며 파일을 삭제하지 않는다. 파일 생성이 확인된 뒤 추가 검증이 실패하면 applied를 유지한다. 손상 파일을 자동 초기화하거나 재생성하지 않는다.

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
| SETUP_AUTH_STORAGE_FAILED | 5 | inspect_private_session: 세션 파일·권한·실행 중 브로커 확인 |
| SETUP_CANCELLED | 130 | inspect_status_before_resume: 완료 단계 확인 후 재개 |

인증 성공 시 검증한 User를 후속 단계용으로 보관한다. 작업 04 종료 당시에는 select_team이 STEP_UNAVAILABLE/종료 3이었다. 작업 05에서 팀 선택을 연결했으며 현재 중단 단계는 bind_project다. 회원가입·Agent 등록·Host 전체 연결은 후속 작업이다.

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

대화형 선택은 waiting_input 진행 이벤트에 descriptor와 teamChoices만 전달하고 해당 단계만 재개한다. 후보 이름은 기존 출력 어댑터의 제어/bidi 문자 escape를 적용한다. 입력 취소는 종료 130이며 인증 단계의 완료 상태를 보존한다. 후속 단계에 검증한 Team을 보관하고 결과의 known.userId/teamId를 전달한다. 현재 팀 선택 완료 후에는 미구현 bind_project에서 종료 3이다.

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
