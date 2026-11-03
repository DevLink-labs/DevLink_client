# 중앙 계약 정합화와 두 Agent 컨텍스트 왕복 (S15P31D108-97)

2026-10-30. client 기준 develop 970b5ea066fefb8c52073d153191bb2611ebfb89, 중앙 기준 0e8c2b74e14ff056cdede24711e804647cd044df다. 실행 대상은 격리 loopback Spring Boot와 PostgreSQL 17.11이며 배포 서버를 사용하지 않았다.

## 수정 내용

아래는 2026-10-30 최초 구현 기록이다. 2026-11-02에는 이미 upstream에 반영된 Agent 경로 변경을 중복 적용하지 않았다.

- Agent 확인은 GET /api/v1/agents/me, 목록은 GET /api/v1/agents/teams/{teamId}다. CLI 토큰 검증·기동, 세션 신원 공급, Agent 확인·목록 중계와 fixture를 갱신했다. 목록의 teamId는 중앙으로 확인한 세션 principal에서만 가져온다.
- register_agent는 CLI가 생성·토큰 발급한 기존 중앙 Agent 확인 및 로컬 세션 bind다. 신규 생성·수정 기능이라고 설명하지 않는다.
- update_task_status는 version을 필수로 받는다. FAILED에는 failureReason과 partiallyModified를 명시하며 IN_PROGRESS에는 실패 필드를 거부한다.
- complete_task는 version/resultText가 필수이며 taskFiles[{relativePath,changeType,diffText?}]는 선택이다. 중앙 계약대로 생략/null은 빈 파일 목록으로 저장되므로 변경 파일이 있으면 반드시 명시한다. 중앙에 별도 verification DTO가 없으므로 호출자가 요약·실제 검증 결과·미실행 항목을 resultText에 명시한다. 구형 result.summary/changedFiles/verification은 거부한다.
- 버전은 Java Long 범위의 정규 십진 문자열 또는 JS 안전 정수만 받는다. 생성 응답을 포함한 Rust 출력 version은 문자열로 내보내 정밀도를 보존한다. 변경 전에 최신 상세 version을 조회하며 자동 조회/재전송하지 않는다.
- 컨텍스트·실패 사유·완료 텍스트·파일 diff의 공백과 개행을 보존한다. REJECTED 목록 필터와 알려진 requestKey의 명시적 전달을 지원한다.
- TASK_VERSION_CONFLICT, CENTRAL_PROTOCOL_ERROR, CENTRAL_RESULT_UNKNOWN을 안전한 오류로 보존한다. 알려진 taskId/requestKey와 unknown/rejected 의미를 유지한다.
- Codex/Claude 지침, workflow 예제 및 SDK 회귀를 함께 갱신했다.

## 실제 검증

scripts/verify-central-roundtrip.mjs는 사용자·Agent 두 명을 새로 생성하고 두 개의 전용 로컬 프로필·프로젝트를 사용한다. localPort=0을 설정하며 기존 사용자 전역 설정이나 팀 데이터를 사용하지 않는다. 단, Rust의 기본 소유권 잠금은 운영 코드와 동일하게 OS 사용자 데이터 디렉터리의 DevLink/broker-owners를 사용하며 해시 이름의 빈 잠금 파일이 남을 수 있다. 전역 자격·Host 설정은 변경하지 않는다.

실제 MCP SDK stdio → MCP → Rust → 중앙 Spring → PostgreSQL 경로에서 다음을 통과했다.

1. 두 사용자의 로그인·팀 생성/참여·Agent 생성·같은 생성 명령의 기존 ID 재사용.
2. 두 브로커 WebSocket Connected 및 MCP register_agent, 중앙 ONLINE 반영, list_agents의 두 Agent 조회.
3. A→B와 B→A 각각 context가 포함된 create_task. 한글·emoji·공백·개행을 포함한 title/description/taskContext 원문을 중앙 상세와 수신함에서 대조.
4. 지정된 상대 수신함에만 메시지 저장, requester/receiver 일치 및 중앙 ACK_RECEIVED 이력 확인. ACK는 사용자 수락과 별개다.
5. 수신자의 IN_PROGRESS 수락, 발신자의 상태 변경 권한 거부, stale version의 TASK_VERSION_CONFLICT와 taskId/requestKey 보존.
6. 수신 프로젝트 내 테스트 응답 파일 작성 후 완료 등록. 양쪽 상세에서 resultText와 파일 유형·diff 원문 및 version 대조.
7. FAILED의 원문 failureReason과 partiallyModified=false 보존.
8. 중앙 API에서 합성 작업을 명시적으로 거절한 뒤 MCP REJECTED 목록 조회. 거절 mutation은 MCP 도구로 제공된 것으로 기록하지 않는다.
9. MCP 자식 프로세스와 두 소유 브로커 종료, 전용 임시 프로필 정리.

최초 실행에서 드러난 Agent 확인 구 경로 실패는 공식 경로 수정 후 해소했다. 중앙의 필수 devlink.websocket.allowed-origins는 시험 기동 인자로 http://localhost:3000을 명시했다. DB는 실제 Flyway migration과 Hibernate validate 설정을 사용한다.

## 재현

중앙과 전용 DB를 미리 기동한다. 스크립트는 loopback HTTP만 허용하고 테스트 사용자·팀·Agent·작업을 생성한다. 비밀번호·토큰은 출력하지 않는다. 중앙/DB 프로세스 자체는 이 스크립트가 관리하지 않는다.

~~~powershell
npm --prefix packages/mcp-server run build
cargo build --manifest-path rust/Cargo.toml -p devlink-cli
$env:DEVLINK_TEST_CENTRAL_URL = 'http://127.0.0.1:<isolated-port>/'
$env:DEVLINK_TEST_CENTRAL_COMMIT = '0e8c2b74e14ff056cdede24711e804647cd044df'
$env:DEVLINK_TEST_CLI = '<absolute-path>/devlink.exe'
node packages/mcp-server/scripts/verify-central-roundtrip.mjs
~~~

## 증거와 한계

- 실제 출력: .planning/mcp-central97/actual-roundtrip-final.log, passed=true, 최종 양방향 taskId 5·6. 앞뒤 공백을 포함한 CLI Agent 이름도 실제 MCP 등록에 성공했다.
- PostgreSQL 직접 대조: 완료 작업 2건, 저장 파일 2건, ACK_RECEIVED 2건, TASK_COMPLETED outbox 2건. Flyway version 6 적용 및 Hibernate validate 사용.
- 최종 MCP 회귀: 104 passed / 0 failed (mcp-tests-complete.log). 공개 requestKey maxLength가 128로 노출되는 결함을 실제 tools/list로 먼저 재현한 후 64로 수정하고 회귀를 추가했다. 공백 키는 네트워크 전 거부한다. 일반 샌드박스의 로컬 네트워크/ACL 제한 실패와 정상 권한 실행을 구분했다.
- Rust workspace/all-targets 순차 회귀: 168 passed / 0 failed / 2 existing opt-in ignored. cargo fmt --all --check, clippy --workspace --all-targets -- -D warnings, git diff --check 통과.
- Claude Code CLI 교차검토 2회: 요청 모델 claude-opus-5-5, effort high, 실제 modelUsage도 두 번 모두 claude-opus-5-5, 종료 코드 0. 1차의 공백 requestKey·Agent 메타데이터 보존·문서·로그/정리 지적을 반영했다. 2차는 차단 결함 없음으로 판정했고 남은 문서 결과 갱신·공개 스키마 제한 표시도 수정·검증했다. 원본은 .planning/mcp-central97/opus-result.json 및 opus-result-round2.json에 보관한다.
- 실제 LLM Codex/Claude Host의 자율 코드 분석·수정 실행은 하지 않았다. 두 Agent 레코드와 실제 SDK/MCP/Rust/중앙 메시지 전달을 검증한 것이다.
- 기존 7개 도구의 중앙 계약을 맞춘 범위다. 누락된 운영 보완·예외 처리 8개 도구 구현 완료로 판정하지 않는다.
- 최종 Jira 완료 판정은 미확인 수용 기준과 구분한다.

## 2026-11-02 최신 develop 통합

기존 f680907은 보존하고 develop 9f6b85f0b4042d8786924863ba4c20b3a91a049d 위에 -97 변경만 다시 통합했다. 이미 upstream에 반영된 Agent 경로 수정은 중복 변경하지 않았고, PR #27의 TASK_CREATED 파싱·ACK 보강과 REJECTED 생성 응답 회귀 및 새 기본 서버 URL을 유지했다. 생성 version은 문자열로 반환하고 Agent 목록의 팀은 인증된 principal에서 정규 Long ID로 확인한다.

최종 계약 대조에서 RESOURCE_NOT_FOUND가 BROKER_REJECTED로 바뀌는 결함을 실제 SDK 회귀로 재현했다. RESOURCE_NOT_FOUND/REQUEST_KEY_CONFLICT/BROKER_STORAGE_FAILED를 허용된 오류로 소비하여 taskId/requestKey와 unknown/rejected를 보존한다. 서버 원문 메시지는 복사하지 않고 자동 재시도도 하지 않는다.

이 절 위의 104개·168개 및 실제 중앙 결과는 2026-10-30 증거다. 이번 통합은 server 746cdb4fe939f2ef61e8fbcd33d6d1d0b318c602와 별도 PostgreSQL/Flyway 환경으로 다시 검증하고, 최종 결과와 Opus 모델·PR은 2026-11-02 Notion 검증 기록에 남긴다. 실제 LLM Host의 코드 수정 수용은 -66에서 별도 수행한다.

새 기본 URL을 테스트가 따라 운영 서버로 접속하지 않도록 CLI 생명주기 fixture는 기본 serverUrl을 명시적 loopback으로 설정한다. WebSocket/API fixture가 있는 시험은 자신의 격리 주소를 다시 지정한다. 운영 기본 URL은 변경하지 않는다.
