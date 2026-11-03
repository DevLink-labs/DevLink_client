# 2026-10-29~30 로컬 세션 구현·검증 이력

이 문서는 당시 코드·환경·검토 상태를 보존한 과거 기록이다. 미구현·차단·검토 대기 표현은 현재 상태가 아니다. 현재 계약과 결과는 [문서 안내](../README.md)·[검증 요약](../verification.md)을 따른다.

최신 상태(2026-10-30): 중앙 `02de434`의 확장 READY와 현재 클라이언트 `d4c3754`로 실제 WebSocket 신원·CLI/SDK 세션·PostgreSQL presence·생산 lease 90초 만료 검증이 통과했다. 기존 `/api/v1/agent/context` 공급부도 유지한다. 아래의 blocked·미검증·미커밋 표현은 각 실행 시점의 기록이며 문서 마지막의 2026-10-30 결과가 우선한다. 이번 검증은 실제 Codex/Claude Host·전체 업무 왕복·-91 bootstrap 자동화의 재검증이나 지정 검토를 포함하지 않는다.

2026-10-29. ADD05 / mvp-gap-20261029의 진행 가능한 로컬 범위를 기존 구현 위에 보완한다. 작업 목록·완료·결과 API, MCP 복구 알고리즘, 중앙 도메인과 Agent 생성은 추가하지 않는다.

## 이번 변경

- 세션 control 경로의 신원/Agent 불일치 응답을 `422 FORBIDDEN`, `scope=domain`, `outcome=rejected`로 맞춘다. 기존 403은 실제 MCP 소비자에서 `LOCAL_AUTH_FORBIDDEN`으로 오분류돼 로컬 자격을 영구 중단시켰다. Origin의 403과 토큰 오류의 401, 업무 경로의 기존 403 매핑은 유지한다.
- 기존 Rust HTTP/CLI 회귀의 신원 거부 상태 코드를 갱신한다.
- 실제 SDK 종료에서 DELETE가 400으로 거부되는 문제를 발견했다. Node HTTP의 DELETE 본문이 올바르게 전송되도록 MCP 전송부에 UTF-8 바이트 기준 Content-Length를 명시한다. 세션 복구 알고리즘은 변경하지 않는다. SDK 종료 후 Rust OFFLINE 검사가 이 회귀를 검출한다.
- 실제 MCP SDK → 실제 MCP BrokerClient → 실제 Rust HTTP/SessionManager의 회귀를 추가한다. 중앙 신원은 테스트에서 설정하며, 업무 REST는 기존 fixture를 재사용한다. 이 fixture의 업무 경로/인증은 운영 중앙 계약을 증명하지 않는다.
- SDK 회귀는 최초 세션 발급, 다른 Agent claim의 도메인 거부, fresh 교체, 동일 instance recover 멱등성, 늦은 recover 충돌, 교체된 MCP의 업무 거부, 이전 클라이언트 종료 후 새 소유자의 heartbeat, 명시 해제, 실제 90초 lease 만료 거부, 다음 명시 호출의 recover를 확인한다. 자동 heartbeat는 300초로 설정하고 자동 retry는 끄되 provider의 90초 lease는 변경하지 않는다.
- 중앙 fixture 호출은 명시적으로 성공한 3회만 허용하며 신원 위조·교체·만료 실패가 업무 생성으로 이어지지 않는지 확인한다. 종료 뒤 실제 CentralState의 OFFLINE도 확인한다.

## 재현

저장소 루트에서 Node 24 이상과 기존 설치 의존성을 사용한다. 별도 임시 MCP 빌드를 rust/target 아래에 생성한다.

```powershell
node packages/mcp-server/node_modules/typescript/bin/tsc -p packages/mcp-server/tsconfig.json --outDir rust/target/mcp-session-build
$env:DEVLINK_MCP_BUILD = Join-Path (Get-Location) 'rust\target\mcp-session-build'
cargo test --manifest-path rust/Cargo.toml -p devlink-broker --test task_creation actual_mcp_session_ownership_expiry_and_identity_rejection -- --ignored --nocapture
cargo test --manifest-path rust/Cargo.toml -p devlink-broker --lib --test session_api -- --test-threads=1
```

SDK 테스트는 91초의 실제 대기를 포함하므로 명시적으로 실행하는 ignored 테스트다. 기본 cargo test에서 제외됐다는 사실을 통과로 해석하지 않는다. SDK와 MCP 사이 transport는 in-memory이며 Rust까지는 실제 loopback HTTP다. 실제 Codex/Claude Host 실행의 증거는 아니다.

## 수용 기준별 근거

| 항목 | 증거 | 남은 경계 |
| --- | --- | --- |
| 4경로, localToken, requestId, bind | session_api.rs HTTP 회귀, 기존 CLI 회귀 | 실제 중앙 인증 공급 |
| fresh/recover, old heartbeat/DELETE, lease 90초 경계 | sessions.rs 단위 및 session_api.rs HTTP 회귀 | 실제 중앙 재시작·교체 |
| SDK 최초 세션·신원/교체/만료 거부 | task_creation.rs의 actual_mcp_session_ownership_expiry_and_identity_rejection | 중앙 신원/업무 fixture 사용 |
| set_presence → AGENT_PRESENCE | 기존 session_api.rs의 실제 로컬 WS 회귀 | 실제 중앙·DB 상태 일치 미검증 |
| 중앙 도메인 업무를 임의 FAILED로 만들지 않음 | 세션 처리와 업무 mutation 분리, 거부 시 중앙 호출 수 검사 | 실제 중앙 업무 상태 조회 대조 미검증 |

## blocked / 미검증

- **중앙 신원 공급 blocked:** [중앙 30b7d70 핸들러](https://github.com/SSAFY-URAYAHA/DevLink_server/blob/30b7d7073150771b2e83877d7edee71baee7c56f/src/main/java/com/devlink/server/handler/DevLinkWebSocketHandler.java)의 READY는 사용자/팀을 보내지 않는다. 현재 Rust 공급부는 확장 READY를 요구한다. `/api/v1/agent/context` 제공과 Rust 소비 연결 완료는 다르다. 사용자/팀을 config나 요청에서 추측하지 않는다.
- **-91 bootstrap 선행:** Agent 생성/기존 Agent 검증/토큰 발급·저장·재사용과 실제 현재 연결의 신원 일치 증거는 이 로컬 세션 변경에서 완료로 판정하지 않는다.
- **실제 중앙·DB 및 Host 미검증:** 중앙 presence 수신·DB 갱신 소스가 존재해도 이 SDK fixture가 실제 중앙 연동을 증명하지 않는다. 전체 Rust 통합은 -52, 최종 A→B/B→A는 -66에서 재사용한다.
- **커밋 전 검토 blocked:** 현재 PATH와 사용자 기본 설치 위치에서 Claude Code CLI를 찾지 못했다. 요청된 `claude-opus-5-5`, effort high 교차검증을 대체 모델로 통과 처리하거나 검토 없이 구현 커밋하지 않는다.
- Notion 요구사항 원문 2개는 연결에서 object_not_found로 읽지 못했다. 사용자 제공 수용 기준과 저장소 계약을 기준으로 대조했으며 요구사항 ID별 원문 확인은 남아 있다.

## 이번 실행 결과

Windows / Node v24.18.0. 최종 변경에서 다음을 실행했다.

| 검사 | 결과 |
| --- | --- |
| Rust broker 단위 | 40 통과 |
| 세션 HTTP/WebSocket | 7 통과 |
| 기존 작업 생성·세션 권한 회귀 | 9 통과 |
| SDK→MCP→Rust 명시 실행 (`actual_mcp_ -- --ignored --nocapture --test-threads=1`) | 2 통과, 98.06초. 세션 90초 만료/복구/종료 OFFLINE 및 기존 5개 업무 시나리오 |
| 실제 CLI READY 신원·세션·presence (`cli_uses_ready_identity_for_real_local_sessions`) | 1 통과 |
| MCP `npm test` | 타입 빌드·계약 drift 검사와 기존 100개 통과 |
| 추가 `node --test test/session-delete-wire.test.mjs` | 1 통과. 한글 식별자의 UTF-8 길이 및 종료 중 DELETE 본문 보존 |
| `cargo fmt --all --check`, Clippy 전체 targets `-D warnings`, `git diff --check` | 종료 0. Clippy 실행에는 증분 캐시 hard-link 실패 후 복사하는 환경 경고가 있었음 |

총 대상 Rust 59개와 MCP 101개를 확인했다. 기본 실행에서 ignored인 SDK 2개는 별도 명시 실행으로 통과했으며 전체 Rust 통합(-52) 수행을 뜻하지 않는다. MCP 신규 테스트는 전체 100개 실행이 끝난 뒤 추가해 개별 실행했다.

첫 샌드박스 실행에서는 loopback 연결·파일 권한 관련 실패가 발생해 중단했고, 임시 서버·테스트 자격을 사용하는 외부 실행에서 위 결과를 확인했다. 초기 SDK 회귀는 DELETE 400 때문에 종료 후 ONLINE이 남는 실패를 검출했으며 Content-Length 수정 뒤 같은 검증이 통과했다. 임시 HTTP 진단 로깅은 제거했다. 실제 중앙/DB/Host와 지정 Opus 교차검증은 실행하지 않았다. 커밋·push는 보류했다.

## 2026-10-29 후속 코드 검토·재검증

현재 변경분의 HTTP framing·UTF-8 바이트 길이·크기 제한, 종료 중 DELETE, control 경로의 도메인 거부와 로컬 인증 오류 분리, 기존 업무 응답 코드 보존을 검토했다. SessionManager의 소유권·만료·신원 재확인 및 presence 게시 경로와 회귀 테스트도 대조했다. 추가로 확인된 수정 필요 결함은 없으며 이번 후속 검토에서는 기능 코드를 바꾸지 않았다. 이는 본 에이전트의 코드 검토이고 지정된 Claude Opus의 독립 교차검증을 대체하지 않는다.

- `npm test`: 신규 DELETE wire 회귀를 포함한 101개를 한 실행에서 모두 통과. 타입 빌드와 공유 계약 drift 검사 포함, 실패·건너뜀 0.
- Rust broker 단위 40개, 세션 HTTP/WS 7개, 작업 생성/권한 9개 재통과.
- MCP 소스를 새로 빌드한 뒤 opt-in SDK 2개 명시 실행: 97.52초, 실패 0. 실제 90초 lease·복구·종료 OFFLINE 포함.
- 실제 CLI READY 신원·세션·presence 1개 재통과. 대상 Rust 총 59개.
- 포맷·Clippy `--all-targets -- -D warnings`·diff 검사 종료 0. 기존 캐시 hard-link 복사 경고 존재.
- Claude CLI는 PATH·기본 npm/local/WinGet 위치와 확인 가능한 VS Code/Claude 기본 설치 위치에서도 발견되지 않았다. `claude-opus-5-5`, effort high 검토와 구현 커밋은 계속 보류한다. 실제 중앙·DB·Host 검증 범위는 확대되지 않았다.

## 2026-10-29 Opus 전 실연동 구현 및 검증

### 구현과 검토 보완

- 새 `agent_context.rs`가 현재 중앙 origin의 `/api/v1/agent/context`를 WS와 같은 Agent Bearer로 조회한다. 기본 CLI에 연결했으며 READY wire는 바꾸지 않았다. 자세한 신원 정책은 [현재 신원 공급](../ready-identity.md)의 첫 절을 따른다.
- claim/bind/heartbeat에서 매번 조회하고 현재 WS Agent 및 선택적 READY user/team과 대조한다. generation과 조회 전 identity를 응답 적용 직전에 비교한다. 권한 응답과 레지스트리/presence 사이에는 기존 잠금 및 subject epoch 검사를 유지한다.
- 검토에서 인증 거부 뒤 같은 WS를 다시 legacy READY로 취급할 수 있는 경계를 보완했다. 거부·신원 모순은 현재 연결을 AuthFailed로 닫아 새 연결 전 재신뢰하지 않는다. 연결 자체의 네트워크 종료·재시도는 기존 중앙 루프 정책을 따른다.
- 검토에서 legacy 재접속 후 context 확인 전에 만료 타이머가 기존 owner의 ONLINE을 다시 게시할 수 있는 경계를 보완했다. 실시간 공급부가 있는 상태에서는 미확인 신원의 ONLINE을 게시하지 않는다. 같은 신원이 확인되면 lease 안의 owner는 유지한다.
- CLI WS 테스트 대역에 기존 context REST를 추가해 실제 HTTP 인증을 소비하도록 했다. 공급부가 없는 테스트는 기존 READY 단독 검증을 유지한다. 모델 입력이나 로컬 설정으로 user/team을 만들지 않는다.

### 실제 중앙·DB 증거

사용한 중앙 소스는 Git `30b7d7073150771b2e83877d7edee71baee7c56f`의 수정 없는 archive다. `rust/target/session-central/server`에서 bootJar를 빌드했고 테스트 계정만 새 PostgreSQL 17.11 클러스터에 생성했다. 중앙은 loopback 18080, DB는 OS가 선택한 loopback 포트를 사용했다. DB 스키마 준비를 위해 해당 테스트 프로세스에서만 `ddl-auto=update`, Flyway 비활성화를 적용했다. 중앙 소스/기존 DB/전역 서비스 설정은 변경하지 않았다.

`scripts/verify-sessions.mjs`를 최종 CLI 빌드로 실행해 종료 0, `passed=true`, `actualCentral=true`, `actualPostgres=true`, `actualSdkSession=true`를 확인했다. 중앙의 실제 사용자 세션·CSRF로 합성 사용자→팀→Agent→연결 토큰을 만들었으며 토큰을 CLI 인수나 결과에 출력하지 않았다. SQL은 psql로 DB에서 직접 읽었다.

| 실환경 시나리오 | 결과 |
| --- | --- |
| 기존 legacy READY → context → unbound principal | 실제 user/team 일치 |
| 다른 Agent bind → 정상 bind | 위조 거부 뒤 실제 Agent bind 성공 |
| presence ONLINE | DB `agents.status=ONLINE`, `last_seen_at IS NOT NULL` 확인 |
| fresh 교체, 같은 instance recover, 늦은 recover | 교체/멱등/충돌 구분 |
| 이전 owner heartbeat/DELETE | SESSION_REPLACED, 새 owner heartbeat 정상 |
| 명시 DELETE | DB OFFLINE 확인 |
| 실제 SDK→MCP→Rust 최초 세션·close | 검증된 bound 세션과 DB ONLINE→OFFLINE 확인 |
| 실제 브로커 종료·재시작 | 이전 세션 만료, 먼저 recover한 새 instance가 슬롯 확보, 이전 owner의 늦은 recover 충돌 |
| 91초 실제 대기 | 중앙 JSON heartbeat는 유지, 로컬 lease 만료로 DB OFFLINE, old heartbeat는 SESSION_EXPIRED |
| 명시 recover | 새 세션과 DB ONLINE 확인 |
| 실제 중앙 토큰 회전 | 기존 연결 정리·DB OFFLINE, old credential의 heartbeat 거부 |

SDK 테스트는 기존 `list_agents` 도구를 호출해 선행 세션을 수립한다. 해당 로컬 업무 provider는 여전히 404이며 fallback envelope 때문에 도구 결과는 BROKER_PROTOCOL_ERROR다. 이를 목록 성공으로 처리하지 않고 adapter.state=bound 및 SQL ONLINE/종료 OFFLINE으로 세션만 판정한다. 실제 SDK 사용과 실제 Codex/Claude Host 실행은 구분한다.

재현은 이미 격리해서 준비한 중앙 서버와 테스트 DB에 한정한다. psql 대상 DB/역할은 모두 `devlink_session_test`다.

```powershell
$env:DEVLINK_TEST_CENTRAL_URL = 'http://127.0.0.1:18080/'
$env:DEVLINK_TEST_CLI = '<빌드한 devlink.exe 절대 경로>'
$env:DEVLINK_TEST_PSQL = '<격리 PostgreSQL psql.exe 절대 경로>'
$env:DEVLINK_TEST_DB_PORT = '<테스트 DB 포트>'
node rust/scripts/verify-sessions.mjs
```

환경 준비 중 Docker 엔진 미기동, initdb의 한글 경로 인코딩 오류, 고정 DB 포트 bind 거부 및 중앙의 필수 Origin 설정 누락을 확인했다. EDB 공식 바이너리를 임시 ASCII 경로에 두고 OS 선택 포트와 테스트용 Origin 설정을 사용해 해결했다. 실패한 준비 실행을 통합 성공으로 합산하지 않았다. 실제 테스트 종료 뒤 이 작업에서 만든 중앙·DB 프로세스와 임시 자격을 정리한다.

정리 완료: 테스트 중앙 프로세스는 PID와 실행 jar 경로를 대조한 뒤 종료했고 PostgreSQL은 전용 data 경로의 pg_ctl로 정상 종료했다. 임시 DB·자격과 PostgreSQL 실행 파일을 제거했다. 수정 없는 중앙 소스 스냅샷·빌드 jar·로그는 ignored `rust/target/session-central`에 남긴다. 전역 서비스·기존 DB는 변경하지 않았다.

### 회귀와 남은 항목

- Rust workspace 회귀: 단위 40 + context 초기 4 + 세션 7 + 작업 생성 9 + 수신함 3 + CLI auth 8 + CLI broker 33 + CLI teams 5 = 109개 통과, SDK 2개는 기본 실행에서 ignored.
- 후속 검토 보완 뒤 broker 단위·context·세션 51개를 재검증했다. 이후 context의 크기 제한/deadline 및 redirect 자격 비전달 2개를 추가하고 context 6개 전부 통과했다. 서로 다른 기본 회귀 테스트 총수는 111개다. 마지막 보완 뒤 전체 workspace를 다시 실행했다는 의미는 아니다.
- MCP 전체 101개 통과는 직전 기록과 같다. 이번에는 MCP 기능 코드를 추가 변경하지 않았다. 기존 SDK 두 회귀도 별도 명시 실행해 96.33초, 2개 통과를 확인했다.
- 포맷·전체 targets Clippy `-D warnings`·diff 검사 통과. 캐시 hard-link 복사 환경 경고가 있다.
- **미완료:** -91의 CLI Agent bootstrap 자동화, 실제 중앙 프로세스 재시작 및 두 실제 브로커 경쟁 연결 전체 시나리오, 실제 Host·A→B/B→A(-66), 지정 Opus 검토 및 커밋. 실제 로컬 브로커 재시작과 중앙 프로세스 재시작은 구분한다. 전체 Rust 통합의 최종 판정은 기존 -52 범위에 남긴다.

## 2026-10-29 재검토: legacy READY의 A → B → A 소유권 복원 차단

추가 코드 검토에서 실제 결함을 발견했다. context 공급부가 있는 legacy READY의 신원 누락을 모두 같은 재접속으로 처리해, 인증된 Agent가 12 → 13 → 12로 바뀌되 중간 Agent의 context 조회가 없으면 기존 owner가 유지됐다. 새 `legacy_agent_a_b_a_cannot_restore_the_original_owner` 테스트에서 이전 세션 heartbeat가 `Ok(())`를 반환하는 실패를 먼저 재현했다.

`SessionHandle::set_identity`의 보존 조건을 **직전 검증된 subject와 같은 Agent의 legacy 재접속**으로 제한했다. Agent ID가 바뀌면 subject epoch를 증가시켜 이전 세션을 무효화한다. 수정 뒤 새 회귀의 old heartbeat/DELETE는 모두 SESSION_EXPIRED이며 기존 동일 Agent 재접속 회귀도 통과했다.

전체 회귀 중 기존 CLI 테스트 하나가 context 401 뒤 상태를 Connected로 기대해 실패했다. 이전 구현에서 도입한 AuthFailed 정책에 맞춰 기대값을 수정하고 해당 테스트를 별도 재실행해 통과했다. 이 수정은 실패를 숨기는 완화가 아니라 인증 거부 상태가 보존됨을 검사하는 변경이다. MCP SDK 두 회귀는 96.25초, 2개 모두 통과했고 MCP 전체 101개도 한 번의 실행에서 통과했다.

이번 Rust 실행은 workspace 기본 회귀 중 CLI broker까지 진행했고 기대값 1건 실패로 중단됐다. 해당 1건 수정 후 개별 통과했고, 중단으로 실행되지 않은 CLI teams 5개도 별도 실행해 37.04초, 실패 0을 확인했다. 기본 회귀의 대상 총수는 112개(단위 40, context 7, 세션 7, 작업 생성 9, 수신함 3, CLI auth 8, CLI broker 33, CLI teams 5)이며 분할 재실행을 합쳐 모두 최종 통과했다. SDK 2개는 이 기본 개수에 포함하지 않는다. 최초 workspace 명령이 종료 0이었다고 표현하지 않는다. 최종 포맷·Clippy 전체 targets·diff 검사도 종료 0이며 기존 캐시 파일 복사 환경 경고는 남아 있다.

실제 중앙·PostgreSQL 환경은 이전 실행 뒤 정리돼 이번에 다시 기동하지 않았다. 이전의 실제 중앙/SQL 성공 증거와 이번 실제 Rust + 테스트 서버 회귀 증거를 분리한다. 지정 Opus CLI 검토와 커밋은 진행하지 않았다. 기존 README 사용자 변경은 수정하지 않았다.

## 2026-10-29 Rust 밖 변경 정리

사용자의 원복 요청에 따라 루트 README의 표 정렬 변경과 MCP 계약 문서에 추가한 갱신 안내를 HEAD로 복원했다. README 변경은 이 에이전트가 만든 변경이 아니었으며, 내용 변경 없이 정렬만 달랐음을 확인한 뒤 이번 요청 범위에서 복원했다.

MCP DELETE wire 회귀 파일은 `packages/mcp-server/test`에서 `rust/crates/devlink-broker/tests/support/session-delete-wire.test.mjs`로 옮겼다. 기존 MCP 소스 빌드 후 저장소 루트에서 다음과 같이 별도 실행한다.

```powershell
node --test rust/crates/devlink-broker/tests/support/session-delete-wire.test.mjs
```

이후 MCP의 기본 npm test 대상은 기존 100개이고 옮긴 wire 회귀 1개는 별도다. 위의 101개 통과 기록은 이동 전 실제 실행 기록이다. Rust 밖에 남긴 변경은 `packages/mcp-server/src/broker/http.ts`의 Content-Length 수정뿐이다. 이를 제거하면 Node DELETE의 requestId/instanceId 본문이 Rust에 전달되지 않아 세션 해제와 즉시 OFFLINE 검증이 실패하므로 기능 보존을 위해 유지했다.

## Gemini 리뷰 후속 수정 (기준 커밋 e69f363)

사용자가 제공한 Gemini 아르곤 리뷰의 4건을 재현한 뒤 수정했다. 이번 후속 변경은 전부 rust/ 안에 있으며 기존 커밋을 다시 쓰거나 새 커밋을 만들지 않았다.

| 리뷰 지적 | 보완 내용 | 회귀 |
| --- | --- | --- |
| 1. legacy 동시 resolve의 보완/401 누락 | 연결 fence는 generation·connection_id·agent_id로 확인하고, 이미 보완된 user/team과 새 응답의 일관성을 별도 검사한다. 동일 연결의 인증 거부는 보완 여부와 무관하게 적용하며 이전 generation의 거부는 새 연결에 적용하지 않는다. | context_concurrency.rs 5개: 200/200, 200→401, 401→200, user/team 모순, 동일 wire ID를 재사용한 새 generation 보존 |
| 2. 재접속 후 첫 heartbeat 전 authorize 실패 | 먼저 로컬 세션/principal을 검사하고, legacy 신원 미확정일 때만 잠금 밖 context 조회 후 최종 검사한다. 이미 보완된 신원은 추가 REST를 호출하지 않는다. | session_validation.rs의 즉시 인가·REST 횟수·조회 중 해제/교체 검증 |
| 3. stale heartbeat가 중앙 장애로 가려짐 | 만료·교체·instance를 REST 전에 검사하고 응답 후에도 최종 재검증한다. 무효화 후 정리된 세션은 중앙 인증 오류보다 SESSION_EXPIRED가 먼저 나올 수 있으며 기존 기대값도 이에 맞췄다. | session_validation.rs의 stale/foreign/released/missing·Reconnecting·조회 중 해제 검증 |
| 4. AuthFailed 후 저장/ACK 가능 | 식별자 제거 및 generation별 인증 무효화 알림으로 WS를 종료한다. TASK_CREATED와 ACK 직전에도 현재 generation·Connected·identity를 검사한다. ACK 네트워크 전송은 제한된 read lock 안에서 수행해 무효화 쓰기와 순서를 보장한다. | auth_invalidation.rs 4개 및 central_auth_tests.rs 1개 |

인증 유효 시 이미 접수해 시작한 디스크 쓰기는 무효화 뒤 완료·보존될 수 있다. 이를 삭제하거나 중앙 업무 실패로 바꾸지 않는다. 다만 무효화 이후 해당 기록의 ACK는 송신하지 않고, 무효화 후 새로 도착한 작업은 저장하지 않는다. 무효화와 I/O 실패가 경합하는 WS 종료도 한 번의 잠금 안에서 AuthFailed 우선 판정과 identity 제거를 수행해 자동 재접속으로 잘못 넘어가지 않게 했다.

독립 코드 검토 중 추가 lease 경계도 발견해 재현·보완했다. 기존 claim/bind/heartbeat는 중앙 신원 잠금을 기다리기 전 시각을 캡처했다. 85초에 heartbeat가 신원을 확인하고 잠금을 기다린 뒤 91초에 처리될 때 기존 코드는 만료 세션을 갱신했다. 최종 상태 변경 closure 안에서 시각을 읽도록 바꿨다. 새 unit 회귀는 생산 lease 90초를 유지하고 테스트 시계만 85→91초로 이동한다. Tokio test-util은 dev-dependency 기능에만 추가했고 Cargo.lock이나 운영 의존성 버전은 바꾸지 않았다.

신규 회귀는 총 17개(동시성 5, 수신 차단 4, 대기 ACK 1, 세션 검증 6, 시계 경계 1)다. 별도 에이전트로 구현과 검토를 분담했으며 이는 지정 Opus나 Gemini가 수정본을 다시 승인한 기록이 아니다.

검증 구간은 실제 Rust 상태·loopback HTTP/WS와 합성 중앙 응답·임시 수신함이다. 이전 실제 중앙 30b7d70/PostgreSQL 실행 기록과 구분하며, 이번 수정에서 실제 중앙/DB를 다시 기동하지 않았다. 실제 Host·-91·-52·-66 및 커밋의 미완료 경계는 유지한다.

최종 실행 결과:

| 검사 | 결과 |
| --- | --- |
| `cargo test --locked --workspace --all-targets` | 최종 종료 0. 기본 회귀 129개 통과, 실패 0, SDK 2개는 기본 실행에서 ignored. 로그는 ignored `rust/target/gemini-followup-tests.log` |
| SDK 2개 명시 실행 | 2 통과, 96.28초. 실제 MCP 소스 빌드와 Rust HTTP, 중앙 응답은 fixture. 마지막 WS 종료 원자화 이후에는 아래 수신/인증 회귀를 포함한 기본 129개를 재실행했다. |
| `npm test` (packages/mcp-server) | 기존 100개 통과, 타입·공유 계약 검사 포함 |
| 옮긴 DELETE wire 테스트 | 1 통과, UTF-8 본문 및 종료 중 DELETE 검사 |
| `cargo fmt --all --check`, 전체 targets Clippy `-D warnings`, diff 검사 | 종료 0. Clippy 초기 새 테스트의 collapsible_if 지적은 수정 후 재통과. 캐시 hard-link 대신 복사하는 환경 경고가 나올 수 있음 |

기본 129개의 구성은 lib 42, agent_context 7, auth_invalidation 4, context_concurrency 5, session_api 7, session_validation 6, task_creation 9, task_receipts 3, CLI auth 8, CLI broker 33, CLI teams 5다. 검토 중 발견한 새 결함을 재현하는 실패와 수정 뒤 최종 통과를 분리했으며, 이전 커밋의 테스트 통과만으로 이 후속 수정의 성공을 판단하지 않았다.

## 2026-10-29 Gemini 수정본 최종 연동 재검증

대상은 `e69f363`에 현재 미커밋 Rust 수정분을 더한 상태다. 사용자가 제공한 Gemini 재검토는 기존 지적 4건의 해결을 확인했고 새 수정 요구는 없었다. 보고서의 Rust 전체 테스트 101개 표기는 앞선 직접 재실행 결과와 다르다. 실제 `cargo test --locked --workspace --all-targets` 결과는 129개 통과, 실패 0, SDK 2개 ignored이며 로그는 ignored `rust/target/gemini-rereview-workspace-tests.log`다. 지정 Opus 검토가 수행됐다는 뜻은 아니다.

이번에는 최신 Rust CLI를 `cargo build --locked -p devlink-cli`로 빌드하고 MCP 소스도 다시 타입 빌드했다. 중앙은 이전에 고정한 `30b7d7073150771b2e83877d7edee71baee7c56f`의 서버이며 최신 develop으로 표현하지 않는다. archive의 commit comment를 확인하고 중앙 소스 168개 파일을 archive와 SHA-256 대조해 불일치 0을 확인했다. 사용한 jar SHA-256은 `DA0888A576251E49933500FFB6BA780FEE31D9048AE99DA47897836F66A7A3E0`이다.

실제 중앙은 별도 loopback 임시 포트, DB는 새 임시 PostgreSQL 17.11 클러스터와 전용 `devlink_session_test` 역할/DB를 사용했다. 합성 사용자를 실제 세션·CSRF로 가입/로그인시켜 팀·Agent·연결 토큰을 발급했다. 이 테스트 중앙 프로세스에만 `ddl-auto=update`, Flyway 비활성화를 적용했으므로 운영 마이그레이션 검증으로 해석하지 않는다. 기존 서버·DB·중앙 소스는 변경하지 않았다.

| 검증 | 이번 실행 결과와 증거 범위 |
| --- | --- |
| SDK MCP → 실제 Rust, 명시 SDK 2개 실행 | 2개 통과, 실패 0, 92.40초. 실제 90초 lease 만료, 소유권 교체, 신원 위조 거부 및 업무 호출/오류 동작을 검사한다. 중앙 업무 응답과 READY 신원은 fixture이므로 실제 중앙·DB 증거와 구분한다. 로그: ignored `rust/target/final-sdk-integration.log` |
| 실제 중앙·PostgreSQL·최신 CLI·실제 SDK | `node rust/scripts/verify-sessions.mjs` 종료 0. `passed`, `actualCentral`, `actualPostgres`, `actualSdkSession` 모두 true. 로그: ignored `rust/target/session-central/final-actual-integration.log` |
| 검증된 principal·bind | 실제 중앙 user/team 일치, 다른 Agent bind 거부, 정상 bind 성공 |
| 소유권 | fresh 교체, 동일 instance recover 멱등, 늦은 recover 충돌, 이전 owner heartbeat/DELETE 거부, 새 owner heartbeat 성공 |
| presence 생산 호출 → 중앙 수신 → 실제 SQL | DB ONLINE 및 last_seen_at 기록, DELETE/SDK 종료 시 OFFLINE 확인 |
| 실제 로컬 브로커 재시작 | 이전 세션 heartbeat 만료, 첫 recover 소유권 획득, 뒤늦은 이전 owner recover 충돌 |
| lease | 실제 91초 대기 후 DB OFFLINE 및 old heartbeat SESSION_EXPIRED, 명시 recover 후 ONLINE |
| 실제 중앙 토큰 교체 | DB OFFLINE 및 기존 자격의 heartbeat 거부 확인 |
| DELETE UTF-8 본문 wire 회귀 | 1개 통과. 초기 샌드박스 loopback 연결 거부 후 같은 테스트를 승인된 외부 실행으로 재검증했다. 기능 코드는 바꾸지 않았다. |

실제 중앙을 사용한 SDK 검증은 선행 세션 수립과 종료, DB ONLINE/OFFLINE을 검사한다. 기존 `list_agents` 로컬 provider는 여전히 404이며 도구 결과는 BROKER_PROTOCOL_ERROR다. `adapter.state=bound`로 세션만 판정했고 목록 도구의 성공으로 처리하지 않았다. 실제 Codex/Claude Host, 중앙 프로세스 재시작, 두 실제 브로커 경쟁 연결 전체 시나리오, -91 bootstrap 자동화, -52 전체 통합 및 -66 A→B/B→A는 이 실행의 완료 범위에 포함하지 않는다.

종료 직전 SQL에서 `status=OFFLINE`, `last_seen_at IS NOT NULL=true`를 다시 읽었다. 이번에 시작한 중앙 PID와 jar 경로를 대조해 종료하고 전용 data 경로의 pg_ctl로 DB를 정상 종료했다. 검증한 임시 경로 안의 DB·자격·PostgreSQL 바이너리/zip을 제거했으며 기존 프로세스는 건드리지 않았다. 정리 결과는 ignored `rust/target/session-central/final-cleanup-result.json`에 저장했다. 최초 환경 시작은 자동 승인 검토의 사용량 제한으로 실행되지 않았고 사용자 재시도 요청 뒤 정상 실행했다. 이를 기능 테스트 실패로 합산하지 않는다.

이번 최종 연동 확인에서 기능 소스 수정이나 커밋은 하지 않았고 이 문서에 검증 결과만 추가했다.

## 2026-10-30 중앙 02de434 확장 READY 실연동 재검증

기준은 클라이언트 `d4c3754`, 중앙 `02de43468e9dd208a4a3cbc9bf142858e315e156`이다. 중앙 원본 develop의 작업 트리가 깨끗한 것을 확인한 뒤 해당 커밋을 `git archive`로 추출하여 `rust/target/ready-02de434/server`에서 Java 21·기존 Gradle 캐시로 격리 빌드했다. 서버 jar SHA-256은 `C9AFB406931BD624231C29D02350616F12C97C4BD21D7DD9AA76A591A6E5DA2D`이다. 현재 Rust CLI와 MCP 소스도 다시 빌드했다.

전용 PostgreSQL 17.11 클러스터·역할·DB와 무작위 loopback 포트를 사용했다. 기존 서버·DB는 변경하지 않았다. 합성 사용자를 실제 세션·CSRF API로 가입/로그인시켜 팀·Agent·연결 토큰을 발급했다. 이 테스트 서버에서만 Hibernate `ddl-auto=update`, Flyway 비활성화를 사용했으므로 운영 스키마 마이그레이션 검증은 포함하지 않는다.

기존 `rust/scripts/verify-sessions.mjs`의 검사 흐름을 보존한 실행용 사본에 READY wire probe를 추가했다. probe는 Java 표준 WebSocket으로 같은 Agent 토큰을 사용해 브로커 기동 전에 실제 첫 메시지를 읽고 종료한다. 토큰은 자식 프로세스의 표준입력으로만 넘겼으며 파일·명령행·출력에 기록하지 않았다. 신원 ID가 포함된 READY 형식 자체는 다음과 같으며 ID 값은 예시다.

```json
{
  "type": "BROKER_READY",
  "payload": {
    "connection_id": "central-session-id",
    "agent_id": 12,
    "user_id": 3,
    "team_id": 1
  }
}
```

| 검사 | 결과 |
| --- | --- |
| 실제 READY wire | `type/payload` 및 payload 4개 필드 일치. connection_id는 비어 있지 않은 문자열, 나머지는 양의 숫자. 발급한 Agent·로그인 사용자·팀과 모두 일치 |
| 세션 생성·bind | 실제 중앙 신원의 문자열 Principal 일치, 다른 Agent bind 거부, 정상 bind 성공 |
| 소유권 | fresh 교체, 같은 instance recover 멱등, 늦은 recover 충돌, 이전 owner heartbeat/DELETE 거부, 현재 owner 갱신 성공 |
| DB presence | bind 후 ONLINE·last_seen_at 기록, DELETE 및 실제 MCP SDK 종료 후 OFFLINE |
| 브로커 재시작 | 이전 세션 heartbeat 만료, 첫 recover 성공, 뒤늦은 이전 owner recover 충돌 |
| 생산 lease | 실제 91초 대기 후 DB OFFLINE·SESSION_EXPIRED, 명시 recover 후 ONLINE |
| 실제 중앙 토큰 교체 | DB OFFLINE 및 기존 자격 heartbeat 거부 |
| 빌드 | 중앙 bootJar·현재 Rust CLI·MCP TypeScript 빌드 모두 종료 0 |
| READY 회귀 | `cargo test --offline --locked -p devlink-broker ready -- --test-threads=1`의 lib 6개·agent_context 1개·session_api 1개 통과 |
| 실행 결과 | wire probe `actualReadyVerified=true`; 세션 검증 `passed/actualCentral/actualPostgres/actualSdkSession=true`, 종료 0 |

READY 회귀 최초 제한 환경 실행에서는 loopback WebSocket accept가 시간 초과했다. 동일 코드를 제한 밖에서 재실행해 8개 모두 통과했다. PostgreSQL의 최초 한글 경로 초기화도 UTF-8 경로 오류로 실패하여 무작위 영문 임시 경로에서 다시 초기화했다. 이 준비 단계 실패를 서버/Rust 기능 결함이나 최종 통과 결과로 섞지 않는다.

실제 MCP SDK 검증의 `list_agents`는 Rust 로컬 provider 미제공으로 예상한 `BROKER_PROTOCOL_ERROR`를 반환한다. adapter의 bound 상태와 DB presence로 세션 성공만 판정했다. 중앙에 Agent 목록 API가 존재한다는 사실과 Rust 목록 도구 연결 성공은 별개다. 실제 Codex/Claude Host, 중앙 프로세스 재시작, 두 브로커의 경쟁 연결 전체 시나리오, 업무 생성·수락·완료·결과 왕복, -52/-66 및 지정 Opus 검토는 이번 검사에 포함하지 않는다.

최종 SQL은 `status=OFFLINE`, `last_seen_at IS NOT NULL=true`였다. 이번에 만든 서버의 PID·jar 경로를 대조해 종료하고 전용 클러스터를 정상 종료했다. 안전 경로를 검사한 뒤 임시 DB·PostgreSQL 바이너리·다운로드 zip을 제거했다. 원본 서버 저장소와 클라이언트 기능 소스는 수정하지 않았으며 커밋·Push하지 않았다.

ignored 실행 증거는 `rust/target/ready-02de434/`에 남겼다: `verification.log`, `runtime.json`, `db-at-end.json`, `cleanup-result.json`, `server-build.log`, `ReadyProbe.java`, `prepare-verifier.mjs`, `verify-actual-ready.mjs`, `setup.ps1`, `run-verification.ps1`, `cleanup.ps1`. 이번 검증으로 현재 서버의 READY 신원 누락을 이유로 기존 세션 업무를 보류할 필요가 없음을 확인했다. -93/-94/-95의 업무 API 구현·수용 완료를 의미하지 않는다.
