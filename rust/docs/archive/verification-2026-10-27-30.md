# 2026-10-27~30 초기 구현 검증 이력

이 문서는 당시 코드·환경·검토 상태를 보존한 과거 기록이다. 미구현·차단·검토 대기 표현은 현재 상태가 아니다. 현재 계약과 결과는 [문서 안내](../README.md)·[검증 요약](../verification.md)을 따른다.

## 2026-10-30: 중앙 02de434 확장 READY·실제 세션 재검증

클라이언트 `d4c3754`와 중앙 `02de43468e9dd208a4a3cbc9bf142858e315e156`의 격리 빌드로 검증했다. 실제 Bearer WebSocket READY의 `connection_id/agent_id/user_id/team_id`와 발급 토큰 신원 일치를 확인했고, 실제 CLI·MCP SDK·PostgreSQL 17.11의 세션 4경로·소유권·재시작 복구·생산 lease 90초 만료·SQL ONLINE/OFFLINE/last_seen_at·토큰 교체 거부가 통과했다. READY 관련 회귀 8개도 같은 작업 세션에서 통과했다. 임시 서버·DB를 종료하고 최종 OFFLINE 및 임시 실행 환경 제거를 확인했다. 기능 소스 수정은 없으며 기록만 갱신했다. 실제 Host·업무 목록 성공·전체 MVP·운영 마이그레이션 검증과는 구분한다. [세부 증거와 실행 범위](session-verification-2026-10-29-30.md)의 2026-10-30 절을 따른다.

## 2026-10-29: Gemini 지적 4건 후속 보완

기준 e69f363의 Gemini 리뷰를 재현해 동시 context 보완/거부 처리, legacy 재접속 즉시 업무 인가, heartbeat 로컬 오류 우선, AuthFailed 후 WS 수신·ACK 차단을 수정했다. 추가로 잠금 대기 중 90초 만료 경계를 넘는 heartbeat의 부활을 막았다. 최종 Rust 전체 129개, SDK 명시 2개, MCP 100개, 별도 DELETE wire 1개와 포맷·Clippy·diff 검사가 통과했다. 실제 중앙·DB 재실행과 수정본에 대한 Gemini/Opus 재승인은 하지 않았다. 변경은 rust/에만 있으며 커밋하지 않았다. [후속 수정 상세](session-verification-2026-10-29-30.md)의 마지막 절을 따른다.

## 2026-10-29: 추가 검토·A→B→A legacy 소유권 차단

legacy READY의 Agent 전환 A→B→A에서 이전 세션이 유지되는 결함을 테스트로 재현하고 같은 Agent 재접속에서만 소유권을 보존하도록 수정했다. 최신 Rust 기본 회귀 112개는 기대값 1건 수정 후 분할 재실행을 포함해 최종 통과했고 SDK 2개(96.25초), MCP 101개, 포맷·Clippy·diff 검사도 통과했다. 최초 workspace 실행의 실패와 개별 재실행을 [세션 검증 기록](session-verification-2026-10-29-30.md)의 마지막 절에 구분했다. 이번에는 실제 중앙/DB를 다시 기동하지 않았다. Opus 검토·커밋은 미실행이다.

## 2026-10-29: 기존 중앙 context 신원 공급·실제 DB presence

기본 CLI에 `/api/v1/agent/context` 공급부를 연결하고 중앙 소스 `30b7d70` + PostgreSQL 17.11 + 실제 CLI/SDK 세션 검증을 통과했다. 세션 4경로·소유권·90초 lease·브로커 재시작 첫 recover·토큰 회전 거부와 SQL ONLINE/OFFLINE/last_seen_at을 확인했다. [상세 증거와 미완료 항목](session-verification-2026-10-29-30.md)의 마지막 절을 따른다. SDK의 list_agents 업무 provider는 미제공이며 세션 성공과 분리했다. -91 자동화·실제 Host/최종 두 사용자 업무 왕복·Opus 검토·커밋은 완료하지 않았다.

## 2026-10-29: 세션 SDK 회귀·DELETE 본문·신원 오류 분류

[세션 스토리 검증](session-verification-2026-10-29-30.md)에 최종 대상 Rust 59개·MCP 기존 100개 및 신규 1개 통과를 기록했다. 실제 SDK→MCP→Rust의 90초 lease 만료·복구와 종료 OFFLINE을 확인했다. 중앙 신원/업무는 fixture이며 중앙 READY 불일치 및 실제 중앙·DB 연동은 blocked/미검증이다. 지정 Claude CLI 교차검증을 실행할 수 없어 구현 커밋은 보류했다.

## 2026-10-29: READY 신원 확장·실제 로컬 세션 공급부

`83169e8` 이후 작업 트리에 합의된 READY user_id/team_id와 ReadyIdentityProvider를 연결했습니다. 중앙에서 받은 숫자 ID를 MCP Principal 문자열로 변환하며, 잘못된/누락 신원·다른 Agent를 거부합니다. 같은 신원의 재접속은 세션을 유지하고, 사용자·팀·Agent 변경과 A → B → A 경합은 subject epoch로 이전 소유권을 무효화합니다. 상세 동작은 [READY 신원 문서](../ready-identity.md)를 참고하세요.

| 검사 | 결과 |
| --- | --- |
| `cargo fmt --all -- --check` | 통과 |
| `cargo clippy --offline --locked --workspace --all-targets -- -D warnings` | 통과 |
| `cargo test --offline --locked --workspace --all-targets -- --test-threads=1` | 단위 36개·세션 통합 7개·수신 통합 3개·CLI 통합 32개, 총 78개 통과. 실패·무시 0 |
| `git diff --check` | 통과 |

Windows MSVC에서 임시 디렉터리와 로컬 Mock으로 검증했습니다. 기존 Windows ACL·로컬 소켓 제한에 따라 테스트는 승인된 샌드박스 외부에서 실행했습니다. 단위 묶음 9.75초, 세션 통합 0.05초, 수신 통합 1.66초, CLI 통합 119.08초입니다. 45초 실제 주기 Heartbeat 회귀, 수신함·ACK·재시작·기존 인증 분리 회귀도 포함합니다.

추가 검증은 숫자 신원 pair·null/부분 누락/범위 거부, legacy READY의 세션 거부, 다른 Agent claim/bind 거부, 같은 신원 reconnect의 세션 유지, 신원 변경 후 이전 세션 heartbeat/delete 차단과 요청 없는 소유권 정리입니다. 실제 WebSocket READY → HTTP claim → bind → ONLINE, 실제 CLI의 claim → heartbeat → delete → OFFLINE도 확인했습니다. toolType은 임의 생성하지 않습니다.

업무 REST 인증은 Agent Bearer 방향을 문서화했으며 토큰 발급·업무/복구 API의 경로와 DTO를 임의로 추가하지 않았습니다. 중앙 서버·MCP TypeScript·의존성은 변경하지 않았고 이번 작업의 커밋·Push는 하지 않았습니다. 실제 운영 토큰·중앙 배포·DB·MCP Host 왕복과 Unix 실행은 미검증입니다.

## 2026-10-29: 작업 수신함·저장 후 ACK

사용자 요청으로 동일 작업 트리를 재검증했습니다. 포맷·Clippy(-D warnings)·전체 73개 테스트·git diff --check가 다시 통과했습니다. 실패·무시 0이며 단위 10.00초, 세션 통합 0.04초, 작업 수신 통합 1.60초, CLI 통합 121.88초입니다. 기존에 확인한 Windows ACL·로컬 소켓 제약에 따라 테스트는 승인된 샌드박스 외부에서 실행했습니다. 실제 중앙·MCP Host 연동은 이번 재검증에도 포함하지 않았고, 기능 코드는 추가 수정하지 않았습니다.

rust/ 작업 트리에 중앙 TASK_CREATED/ACK DTO, `.devlink-runtime/inbox.json` 원자 저장, 중앙 URL·수신 Agent·task_id 중복 판별, 저장 후 ACK와 InboxFailed 상태를 추가했습니다. 생성·수락·완료·실행·MCP 업무 API·운영 신원 공급부는 이번 변경 범위가 아닙니다. [동작·Mock 실행·후속 계약](../task-inbox.md)을 참고하세요.

| 검사 | 결과 |
| --- | --- |
| `cargo check --offline --locked --workspace --all-targets` | 초기 구현 컴파일 통과. 최종 변경은 아래 Clippy와 전체 테스트로 컴파일 검증 |
| `cargo fmt --all -- --check` | 통과 |
| `cargo clippy --offline --locked --workspace --all-targets -- -D warnings` | 통과 |
| `cargo test --offline --locked --workspace --all-targets -- --test-threads=1` | 단위 33개·세션 통합 6개·작업 수신 통합 3개·CLI 통합 31개, 총 73개 통과. 실패·무시 0 |

Windows MSVC에서 실행했습니다. 처음 샌드박스 내부 테스트는 기존 테스트까지 파일 접근·ACL·로컬 통신 제한으로 실패했고, 자동 승인된 샌드박스 외부에서 전체 검사를 다시 실행해 통과했습니다. 단위 묶음 9.69초, 작업 수신 묶음 1.59초, CLI 묶음 116.41초이며 기본 45초 Heartbeat 회귀를 포함합니다. 증분 캐시 hard link 대신 파일 복사를 사용하는 환경 경고는 검사 실패가 아닙니다.

추가한 검증은 다음과 같습니다.

- 중앙 wire와 동일한 최상위 event_id의 TASK_CREATED 및 payload 내부 event_id/task_id의 ACK.
- ACK 수신 시 디스크에 이미 존재하는 기록을 별도 Inbox 인스턴스로 확인.
- 재접속·재시작·새 event_id의 재전달에도 수신함 항목 1개와 현재 event_id ACK 유지.
- 중앙 URL·수신 Agent 격리, payload 충돌 시 원본 보존·ACK 차단.
- 손상·누락 snapshot, 잘못된 수신자·저장 대상·용량 초과에서 ACK하지 않음.
- 디스크 writer를 막아도 반복 JSON PING/PONG이 유지되고 저장 완료 전에는 ACK하지 않음.
- 실제 CLI start → 작업 저장·ACK → stop/start → 재수신·ACK와 snapshot 불변 확인.

새 의존성은 추가하지 않았습니다. TypeScript MCP·중앙 서버·공유 계약은 변경하지 않았고 커밋·Push도 하지 않았습니다. 실제 중앙 토큰·DB·MCP Host의 왕복, Unix 실행, 중앙 5초 ACK 제한 아래의 운영 성능은 미검증입니다.

## 2026-10-28: 로컬 세션 API·소유권·만료·presence 구현

기준은 feature/rust/session-api의 `b8bd142` 이후 작업 트리입니다. 사용자 요청에 따라 rust/ 안에서만 구현했고 커밋·Push는 진행하지 않았습니다. 중앙 신원 조회·등록/토큰 발급·업무 API는 이번 범위에서 제외합니다.

세션 생성·bind·heartbeat·해제 HTTP API, MCP localToken 전용 인증, requestId+data/error envelope, 입력 상한·공유 식별자 검증을 추가했습니다. 세션 관리자는 instanceId 멱등성·fresh 교체·recover 충돌·브로커 시계 90초 만료·오래된 갱신/해제 차단을 구현합니다. 요청 전 정리와 1초 주기 만료 태스크를 사용하며, 확인된 bound 소유자 유무로 CentralState presence를 같은 잠금 안에서 갱신합니다.

IdentityProvider는 교체 가능한 비동기 인터페이스입니다. 기본 CLI에는 UnconfiguredIdentityProvider를 연결하므로 생성은 503 CENTRAL_UNAUTHENTICATED로 닫히고 가짜 principal이나 ONLINE을 만들지 않습니다. 테스트 내부에서만 검증된 신원을 주입해 성공 흐름을 확인합니다. 실제 구현 계약과 후속 연결은 [로컬 세션 API](../local-sessions.md)를 참고합니다.

| 검사 | 결과 |
| --- | --- |
| `cargo check --offline --locked --workspace --all-targets` | 통과 |
| `cargo fmt --all -- --check` | 통과 |
| `cargo clippy --offline --locked --workspace --all-targets -- -D warnings` | 통과 |
| `cargo test --offline --locked --workspace --all-targets -- --test-threads=1` | 단위 27개·세션 HTTP/WebSocket 통합 6개·CLI 통합 30개 통과, 실패·무시 0 |

Windows MSVC에서 ACL·로컬 HTTP/WebSocket 전체 테스트를 승인된 샌드박스 외부에서 순차 실행했습니다. 세션 HTTP 묶음은 0.10초, CLI 묶음은 131.51초였으며 기존 45초 실제 주기 Heartbeat 회귀도 포함합니다. 컴파일·Clippy에는 증분 캐시 hard link 대신 파일 복사를 사용하는 환경 경고가 있었고 검사 종료 코드는 0입니다.

새 검증은 다음을 포함합니다.

- 공유 principal/session 신원 fixture, Unicode 128개 경계와 null/추가 필드 거부.
- 동일 인스턴스 동시 claim 멱등성, 서로 다른 recover 경쟁의 단일 소유자, fresh 교체·이전 세션의 갱신/해제/재claim 차단.
- 재시작 전 소유권 미보존, 브로커 시계 90초 경계·갱신, 요청 없는 백그라운드 만료. 경계 검사는 내부 monotonic 시각으로 수행하고 주기 정리는 테스트 내부 lease만 단축했습니다.
- unbound bind 충돌·사용자/팀/Agent 변경 거부, 해제·종료 이후 지연된 신원 응답으로 세션 부활 방지.
- 실제 HTTP의 생성→bind→heartbeat→DELETE, requestId·principal·오류 scope/outcome, 인증·Origin·크기·Content-Type·잘못된 JSON·중복 필드·행위자/시각 덮어쓰기 거부.
- 같은 실제 로컬 WebSocket에서 unbound 생성 OFFLINE→bind ONLINE→해제 OFFLINE 보고.
- 실제 CLI에서 MCP 토큰의 세션 생성은 신원 미구성 503, CLI controlToken은 세션 API 401이며 health·중앙 연결·정상 종료는 유지.

MCP 전체 테스트·릴리스 빌드, 실제 중앙 인증·토큰·DB, 실제 MCP Host와의 업무 왕복은 이번에 실행하지 않았습니다. 기존 TypeScript·중앙 서버·공유 계약 파일은 수정하지 않았고, 새로운 의존성이나 운영용 신원 우회 설정은 추가하지 않았습니다.

## 2026-10-28: 실시간 45초 연결 유지 자동 회귀 추가

`feature/rust/broekr_sessionapi`의 `6e6fd78` 이후 작업 트리에 `json_heartbeat_keeps_cli_connected_for_45_seconds_without_reconnect`를 추가했습니다. 앞서 별도 실행으로 확인한 45초 유지 검증을 Rust CLI 통합 테스트로 저장했으며 `#[ignore]` 없이 기본 전체 테스트에 포함합니다.

실제 CLI 실행 파일과 테스트 전용 설정·토큰·동적 포트를 사용합니다. 운영의 JSON PING 20초·PONG 대기 20초와 테스트 Mock의 JSON PING 무수신 만료 40초를 단축하지 않습니다. Mock은 유효한 JSON PING만 만료 시각을 갱신하고 같은 ID의 PONG을 응답합니다. presence·WebSocket 제어 Ping/Pong은 만료 시각을 갱신하지 않으며 무수신 시 Close 4001로 종료합니다.

45초 동안 250ms 간격으로 health의 Connected·같은 PID/instanceId를 확인하고 WebSocket 연결 횟수 1회·만료 0회를 검사합니다. 약 20초·40초에 같은 연결에서 서로 다른 ping_id를 받았는지와 정상 stop 후 잠금 해제도 확인합니다. 중간에 끊겼다가 재접속해 Connected로 복구된 경우도 연결 횟수 검사로 실패합니다. 테스트만 다시 실행하려면 `rust/`에서 다음 명령을 사용합니다.

```powershell
cargo test --offline --locked -p devlink-cli --test cli_broker json_heartbeat_keeps_cli_connected_for_45_seconds_without_reconnect -- --exact --test-threads=1
```

| 검사 | 결과 |
| --- | --- |
| `cargo check --offline --locked --workspace --all-targets` | 통과 |
| `cargo fmt --all -- --check` | 통과 |
| `cargo clippy --offline --locked --workspace --all-targets -- -D warnings` | 통과 |
| `cargo test --offline --locked --workspace --all-targets -- --test-threads=1` | 단위 17개·통합 29개 통과, 실패·무시 0. 새 45초 테스트 포함 |

Windows MSVC에서 전체 테스트를 승인된 샌드박스 외부에서 순차 실행했습니다. 통합 테스트 묶음은 142.88초가 걸렸습니다. 컴파일·Clippy에는 증분 캐시 hard link 대신 파일 복사를 사용하는 환경 경고가 있었고 종료 코드는 0입니다. 실제 중앙 배포 서버·운영 토큰·DB를 사용하는 검증은 아닙니다. 기존 스테이징은 유지했으며 이번 추가분은 별도 커밋·Push하지 않았습니다.

## 2026-10-28: JSON Heartbeat·4001 만료 후 재접속 수정

기준은 `feature/rust/session-api`의 `6e6fd78` 이후 작업 트리입니다. 현재 로컬 중앙 서버의 `DevLinkWebSocketHandler`·`ConnectionRegistry`·`BrokerHeartbeatService`·application.yml을 읽어 JSON PING/PONG, 동일 ping_id 응답, PING 무수신 기본 40초 만료와 Close 4001을 확인했습니다. 서버 파일은 수정하지 않았으며 이번 변경은 모두 `rust/`에 있습니다.

Rust는 READY 후 JSON HEARTBEAT PING을 20초 간격으로 보내고 각 ping_id와 일치하는 PONG을 최대 20초 대기합니다. 응답 대기 중에는 새 요청으로 기존 대기를 덮어쓰지 않으며 다른 메시지·제어 프레임·잘못된/이전 ID의 PONG으로 제한 시간을 연장하지 않습니다. 4001과 PONG 만료는 재접속 설정을 따르고 HTTP 401/403, Close 4002·1008은 자동 재접속을 중단합니다. 개발용 Mock도 JSON PING/PONG·40초 만료·4001로 맞췄습니다.

| 검사 | 결과 |
| --- | --- |
| `cargo check --offline --locked --workspace --all-targets` | 통과 |
| `cargo test --offline --locked --workspace --all-targets -- --test-threads=1` | 단위 17개·통합 28개 통과, 실패 0 |
| `cargo fmt --all -- --check` | 통과 |
| `cargo clippy --offline --locked --workspace --all-targets -- -D warnings` | 통과 |

Windows MSVC에서 실행했으며 ACL·HTTP·WebSocket을 사용하는 전체 테스트는 승인된 샌드박스 외부에서 실행했습니다. 컴파일·Clippy 시 증분 캐시 hard link 실패를 파일 복사로 대체하는 환경 경고가 있었으며 검사 종료 코드는 0입니다.

새 단위 테스트는 실제 로컬 WebSocket 서버와 운영과 같은 연결 루프를 사용하고, private HeartbeatTiming만 PING 40ms·PONG 대기 120ms(서버 만료를 먼저 유도하는 경우 500ms)로 단축합니다. 공개 CLI·환경변수에 테스트 시간 옵션을 추가하지 않았습니다. 다음을 확인했습니다.

- 유효한 JSON PING/PONG을 반복하면 서버의 최초 만료 시간 250ms를 넘겨 같은 연결이 유지되며 ping_id는 매번 새 UUID입니다. 하트비트만으로 미확인 presence를 ONLINE/OFFLINE으로 바꾸지 않습니다.
- 잘못된 ID의 PONG 또는 이전 PONG 재사용, 잘못된 형식, 일반 JSON, 제어 Ping/Pong을 계속 받아도 미응답 PONG 대기가 만료됩니다. 대기 중 새 PING을 보내 이전 요청을 덮어쓰지 않습니다.
- 서버의 만료 감시가 첫 연결을 4001로 종료하면 식별자를 정리하고 백오프 후 새 READY·새 ping_id로 복구하며 확인된 presence를 다시 보고합니다.
- 실제 CLI 통합 테스트는 Close 4001에서 autoReconnect=true이면 재접속·Connected, false이면 Disconnected·추가 연결 없음, HTTP 401/403은 인증 실패·재시도 중단을 각각 확인합니다. 기존 4002·1008 중단 테스트도 통과했습니다.

이 검증은 실제 로컬 소켓과 서버 계약을 재현한 Mock을 사용한 회귀 검증입니다. 실제 중앙 배포·운영 토큰·DB 갱신, 20초/40초 운영 주기의 실환경 장시간 유지, Rust 세션·업무 API 및 두 사용자 업무 왕복은 검증하지 않았습니다. MCP 전체 테스트·일반/릴리스 빌드·준비 상태 재현 도구는 이번 수정에서 재실행하지 않았습니다.

## 2026-10-28: 작업 현황 점검에서 재확인

`feature/rust/broekr_sessionapi`의 HEAD `037810b`와 미커밋 presence·통합 검사 보완을 포함한 로컬 작업 트리에서 다음 결과를 재확인했습니다. 아래 검사는 문서 갱신 직전의 작업 현황 점검에서 실행했으며 문서 수정 후 기능 검사를 새로 실행했다는 의미는 아닙니다.

| 검사 | 결과 |
| --- | --- |
| `cargo check --offline --locked --workspace --all-targets` | 통과. 증분 캐시 hard link 실패 시 파일 복사로 대체하는 경고 발생 |
| `cargo fmt --all -- --check` | 통과 |
| `cargo test --offline --locked --workspace --all-targets -- --test-threads=1` | 단위 13개·통합 27개 통과, 실패 0 |
| `npm.cmd test` (`packages/mcp-server/`) | TypeScript 빌드·공유 계약 검사·100개 테스트 통과, 실패 0 |

Rust와 MCP의 ACL·로컬 HTTP/WebSocket 테스트는 승인된 샌드박스 외부 실행에서 통과했습니다. MCP 테스트의 샌드박스 내부 시도에서는 로컬 통신·권한 검사가 포함된 항목에 실패가 발생해 중단했고, 외부에서 전체를 재실행했습니다. 이 점검에서는 Clippy·일반/릴리스 빌드·Rust 준비 상태 재현 도구를 다시 실행하지 않았습니다. 해당 결과는 아래의 기존 기록과 구분합니다.

이번 테스트는 로컬 테스트 서버·가짜 브로커·임시 자격 파일을 사용했습니다. 실제 중앙 인증·DB 갱신·검증된 사용자/팀 신원 전달·Rust 세션/업무 API·두 사용자 업무 왕복의 통과를 의미하지 않습니다. GitHub 연결에서 저장소 검색 접근 오류가 발생해 원격 PR·이슈·최신 브랜치 상태는 확인하지 못했습니다.

## 2026-10-28: 통합 검사 도구 호환성·presence 변경 보고 보완

기준 브랜치는 `feature/rust/broekr_sessionapi`, 병합 기준은 `037810b`다. 격리 Rust 재현 도구의 중앙 주소를 `/ws/broker`로 변경하고, MCP 준비 상태 검사기에 `Replaced`·`ProtocolViolation`을 추가했다. 두 상태도 유효한 health로 인식한 뒤 `CENTRAL_NOT_CONNECTED`로 보고하고 11개 경로 검사를 계속한다.

Rust는 HTTP 핸들러와 중앙 연결 태스크가 같은 `CentralState`를 공유한다. `set_presence`에 확인된 상태를 게시하면 READY 이후 현재 연결에서 보고하며, 동일 상태는 중복 전송하지 않는다. 연결 전·재접속 중에는 최신 상태를 보존하고 다음 READY 직후 보고한다. 기본 미확인 상태는 유지하며 실제 세션 API의 bind·해제·만료 이벤트 연결은 아직 남아 있다.

Windows MSVC와 Node.js 24 환경에서 다음 검사를 수행했다. ACL·로컬 HTTP/WebSocket 검사는 샌드박스 밖에서 실행했다. 앞선 병렬 검토에서 5초 종료 시간 조건 실패가 관측되어 이번 전체 Rust 검증은 순차 실행했다.

| 검사 | 결과 |
| --- | --- |
| `cargo check --offline --locked --workspace --all-targets` | 통과 |
| `cargo test --offline --locked --workspace --all-targets -- --test-threads=1` | 단위 13개·통합 27개 통과 |
| `cargo fmt --all -- --check` | 통과 |
| `cargo clippy --offline --locked --workspace --all-targets -- -D warnings` | 통과 |
| `npm.cmd test` (`packages/mcp-server/`) | 빌드·공유 계약 검사·100개 테스트 통과 |
| 실제 CLI로 `scripts/verify-rust-readiness.mjs --cli <절대 경로>` 실행 | 자격 파일 valid, health 200, 세션·업무 11개 OPTIONS 404, `blocked` 및 실제 자식 종료 코드 2 확인 |

presence 검증은 실제 로컬 WebSocket 테스트 서버에서 READY 전 전송 보류, 연결 유지 중 ONLINE/OFFLINE 변경, 동일 상태 중복 생략, ACK 없이 연속 보고, 재접속 중 최신 상태 보존·READY 직후 재보고, 종료 후 식별자 정리를 확인했다. 실제 중앙 서버·운영 토큰·DB·실제 Host 세션을 사용한 검증은 아니다. 격리 재현은 중앙 503 서버와 미구현 API를 의도적으로 사용하므로 차단 판정이 예상 결과이며 MVP 통과를 뜻하지 않는다.

## 2026-10-28: PR #11 P2 후속 수정 (S15P31D108-46)

최신 develop `69f155a`에서 확인한 Unix `libc` 의존성 누락과 Windows ACL 검사·기동 시간 예산 충돌을 수정했습니다. Unix 전용 직접 의존성을 workspace manifest와 lockfile에 반영했으며, Windows 시작 대기는 기존 준비 여유 8초에 최대 3회 ACL 검사(각 5초)를 포함한 23초입니다. Unix 시작 대기는 기존 8초를 유지합니다. 권한 검사는 생략하지 않습니다.

Windows MSVC에서 다음 검사를 통과했습니다. 기본 GNU 도구 체인에는 `dlltool`이 없어 설치된 MSVC를 명시했습니다.

```powershell
cargo +stable-x86_64-pc-windows-msvc test --locked --offline --workspace --all-targets
cargo +stable-x86_64-pc-windows-msvc clippy --locked --offline --workspace --all-targets -- -D warnings
cargo +stable-x86_64-pc-windows-msvc fmt --all -- --check
```

단위 7개·통합 22개가 통과했습니다. 신규 `startup_waits_for_valid_slow_acl_checks`는 합성 자격 파일과 동적 포트를 사용하고, 테스트가 실행한 start CLI 자식의 PATH에만 네이티브 PowerShell 프록시를 지정해 브로커가 상속하도록 합니다. 실제 ACL 스크립트마다 3초를 추가한 상태에서 검사 3회가 모두 실행되고, 8초를 넘겨 정상 기동하며 기존 토큰·새 주소·status·stop이 유지되는지 확인합니다. 프록시는 테스트에서 설치된 `rustc`로 임시 디렉터리에 컴파일하며 운영 실행 파일에 지연 옵션을 추가하지 않습니다. 실제 MCP reader 검사와 기존 토큰 유지·회전·권한 분리 테스트도 통과했습니다.

`cargo metadata --locked --offline --no-deps`로 broker의 `libc` 직접 의존성과 `cfg(unix)` 조건을 확인했습니다. 이 Windows 환경에 Unix 실행 도구 체인이 없어 Linux/macOS 전체 빌드·실행은 미검증입니다. 실제 `/v1` 업무 API나 중앙 서버 연동 검증을 의미하지 않습니다.

## 이전 확인: 2026-10-27

검증 환경: Windows x86_64 MSVC, rustc/cargo 1.99.1, Visual Studio 2022 Build Tools 및 Windows SDK. 기능 코드 기준은 `7bfca74`(P2 디렉터리 분리)입니다. P1의 `Cargo.lock` 추적과 `protocol.rs` 포맷 수정도 이 기준에 포함되어 있습니다.

명령 실행 위치는 저장소의 `rust/`입니다. CLI와 브로커를 두 크레이트로 나눈 Cargo workspace 기준이며 빌드 산출물은 저장소의 `rust/target/`에 생성됩니다.

| 검사 | 결과 |
| --- | --- |
| `cargo build --offline --locked --workspace` | 통과 |
| `cargo test --offline --locked --workspace --all-targets` | 단위 테스트 1개, 통합 테스트 15개 통과, 실패 0 |
| `cargo fmt --all -- --check` | 통과 |
| `cargo clippy --offline --locked --workspace --all-targets -- -D warnings` | 통과 |
| `cargo build --release --offline --locked --workspace` | 통과, `target/release/devlink.exe` 생성 |
| `target/release/devlink.exe --help` | 기본 CLI 명령 도움말 확인 |

의존성 캐시가 있는 환경에서 `--offline`을 추가해 실행했습니다. 로컬 통신이 필요한 테스트는 승인된 샌드박스 외부 실행으로 검증했으며, `TEMP`와 `TMP`는 기본 Windows 임시 경로를 사용했습니다.

통합 테스트는 실제 실행 파일의 init/config/start/status/stop, CLI 종료 후 브로커 유지, 동시·중복 실행, 로컬 API 인증·Origin 거부, 강제 종료 후 stale metadata 복구, 포트 충돌, 살아 있지만 Health 인증이 실패한 인스턴스의 오류 처리, 인증 정보 비노출을 확인합니다. Mock 중앙 서버로 인증 성공/거부/4001, 인증 대기 시간 초과·잘못된 JSON, 연결 종료·서버 재시작 후 재연결, 재시도 대기 중 종료를 확인합니다.

P2 회귀 검증은 `runtime.json`, `broker.lock`, `control.lock` 및 대소문자 변형을 설정 파일명으로 사용할 때 설정을 보존하는지 확인합니다. `init`·설정 수정, stale metadata 정리, 토큰 누락으로 실패한 `start`, 실제 브로커 시작·상태 조회·정상 종료를 검사합니다. 단위 테스트는 런타임 게시 후 `RuntimeGuard`가 내부 메타데이터만 삭제하고 설정과 잠금 파일은 유지하는지 확인합니다. 내부 파일은 설정 디렉터리 아래의 `.devlink-runtime/`에 저장됩니다.

## 샌드박스에서 발생했던 실패와 원인

샌드박스 내부에서 작업 폴더의 임시 경로를 사용한 실행은 단위 테스트 1개 통과, 통합 테스트 5개 통과·10개 실패였습니다. 직접 진단한 결과 브로커 프로세스와 런타임 파일은 정상 생성됐지만 `127.0.0.1` TCP 연결이 Windows 오류 `10013`(소켓 접근 거부)으로 차단됐습니다. 이로 인해 8개 테스트는 시작 확인에서, 강제 종료 복구 테스트는 준비 대기에서, 동시 시작 테스트는 성공 여부 검사에서 실패했습니다.

같은 코드를 샌드박스 밖에서 재실행하자 16개 테스트가 모두 통과했습니다. 작업 폴더의 임시 경로와 기본 Windows 임시 경로에서 각각 통과를 확인했습니다. 앞서 샌드박스의 기본 임시 경로에서 발생했던 `Cannot replace JSON file` 오류도 최종 샌드박스 외부 실행에서는 재현되지 않았습니다. 정확한 파일 저장 실패의 하위 OS 오류는 별도로 확정하지 않았습니다.

## 이전 기록과 미검증 범위

2026-10-26 문서에는 통합 테스트 13개와 빌드·릴리스·포맷·Clippy 통과가 기록되어 있었습니다. 위 표는 그 기록을 재사용한 것이 아니라 P1·P2 수정 이후의 최종 실행 결과입니다.

현재 저장소에는 `.github/workflows/`가 없으므로 Windows/Ubuntu CI 구성 및 실행 완료로 표기하지 않습니다. Ubuntu, 실제 중앙 서버 운영 환경, WSS 서버와의 end-to-end 연결, 실제 Agent/MCP 서버 Tool 호출은 이번 검증에 포함하지 않았습니다. MCP 담당자 합의 상태는 [MCP 인터페이스 문서](../mcp-interface.md)에 별도로 표시했습니다.
