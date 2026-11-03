# 작업 수신함·ACK — S15P31D108-95 통합

2026-10-30. 최신 develop의 TASK_CREATED/ACK 타입, Inbox::open/record/list_for, 중앙·세션 코드를 유지하고 닫힌 PR #16의 필요한 CLI·영속성 보완을 통합했다. 이전 PR 전체를 덮어쓰거나 별도 수신함을 병행하지 않는다. ACK는 저장 확인이며 열람·사용자 수락·편집·IN_PROGRESS 전이가 아니다.

후속 -95 보완·실행·교차검토 결과는 이 문서의 마지막 절에 통합했다. 저장 규격과 ACK/수락의 구분을 유지한다.

## 수신과 기록

1. 유효한 BROKER_READY 이후 양의 signed64-bit ID, event_id/prompt/created_at, 인증된 receiver_agent_id를 확인한다. READY team_id가 있으면 팀도 대조한다. 수신 payload를 사용자 신원으로 승격하지 않는다. type이 TASK_CREATED인데 필수 필드 누락·형식 오류로 파싱할 수 없으면 ProtocolViolation으로 연결을 종료하고 저장·ACK하지 않는다.
2. 최대5ms/16건을 묶어 하나의 snapshot으로 저장한다. blocking writer는 Inbox당1개이고 현재 연결은 기록 중 batch1개 + 대기16건까지만 보관한다. heartbeat/control Ping/close 처리는 계속한다. 초과하면 ACK 없이 연결을 닫는 기존 재연결 정책을 따른다.
3. 안전한 경로·기존 private 파일을 확인하고 inbox.lock을 잡는다. 임시 파일의 사용자 전용 권한 설정·검사 → 쓰기·sync_all → 원자 교체 → 최종 ACL 검사·열려 있는 write 핸들 sync_all, Unix 부모 디렉터리 sync를 거쳐야 ACK를 반환한다.
4. ACK 직전 현재 연결 generation과 Agent를 다시 확인하고 같은 소켓으로 해당 event_id/task_id를 보낸다. 이전 generation의 완료는 새 신원/연결을 덮어쓰거나 ACK하지 않는다. 전송에 실패하면 기록을 되돌리지 않는다.
5. batch 안에 충돌·용량·저장 실패가 있으면 batch 전체에 성공 ACK를 보내지 않고 InboxFailed로 닫는다. 원자 교체 전 검증 실패에는 부분 기록도 없다. 교체 후 최종 flush/권한 실패는 결과가 불확실하므로 성공으로 간주하지 않는다.

Windows 파일 점유로 교체가 거부되면 private 임시 파일을 유지해 최대10회(재시도 간50ms, 최대450ms)만 다시 시도한다. 계속 점유되면 ACK 없이 실패하며 원본은 유지한다. 교체 전 실패로 인메모리 store를 영구 오염시키지 않고, 교체 후 검증/flush 실패만 불확실한 게시 상태로 잠근다. 중앙 연결의 실패 후 재시작 정책은 동일하다.

정규화된 중앙 URL + receiver_agent_id + task_id로 사용자 업무 중복을 방지한다. origin + receiver + event_id는 연결 재전달 이력이며 같은 event가 다른 task로 재사용되면 거부한다. 같은 task·payload에 새 event_id가 도착하면 업무는1건을 유지하고 새 영수증만 저장·ACK한다. **따라서 다른 event_id의 재전달은 journal 바이트를 바꿀 수 있다.** 같은 event/task의 완전 중복은 재기록 없이 확인 ACK한다. 실행의 exactly-once를 보장하는 계약이 아니다.

## 파일과 호환

현재 경로는 프로필별 .devlink-runtime/inbox.json이다. origin·Agent가 다른 기록을 함께 보관하되 조회에서 분리한다. 중앙 host alias는 자동 통합하지 않는다.

- 기존 upstream schema_version=1/tasks 파일을 원본 payload·origin·Agent 그대로 읽는다. 기존 첫 event는 해당 task의 dedup 근거로 사용한다.
- tests/fixtures/inbox-v1-d49cdba.json은 d49cdba 원본 Inbox를 별도 빌드·실행해 생성한 파일이다. 기존 writer도 같은 endpoint_url 정규화와 StoredTask 필드를 사용함을 대조했고, 이 원본의 읽기·완전 중복 무변경·다음 기록 업그레이드를 회귀로 확인한다.
- 새 기록/새 영수증을 저장할 때 schema_version=2로 전환하며 tasks와 receipts를 함께 원자 저장한다. 기존 파일의 완전 중복 조회만으로는 업그레이드하지 않는다.
- 구버전 바이너리는 schema2를 읽지 못한다. 다운그레이드 전 private 원본을 별도로 보관·내보내야 하며 새 파일을 구버전 형식으로 임의 변경하지 않는다.
- 최대64MiB·task1024건·추가 receipt16384건을 명시적 clear까지 보존한다. 자동 축출·기간 삭제는 없다. 한도 초과는 ACK 없이 거부하며 이미 아는 event의 확인은 가능하다.
- 닫힌 PR16의 별도 inbox/tasks.json 형식은 최신 develop의 저장 계약이 아니다. 출처/Agent를 추측해 자동 병합·이동하거나 삭제하지 않는다. 필요하면 해당 브랜치와 원본을 보관한 뒤 별도 마이그레이션으로 다룬다.
- 손상/schema/충돌/안전하지 않은 경로나 권한은 자동 초기화하지 않는다. 원본을 private 보관하고 원인을 수정한 후 재시작한다.

stop/start·재연결·토큰 회전으로 수신함을 삭제하지 않는다. 종료 시 새 쓰기를 금지하고, 늦은 blocking writer도 파일 lock 획득 후 stop 상태를 확인한다. 실제 기록 중에는 inbox.lock이 clear와 겹치지 않게 하므로 정지 후 정리된 파일을 이전 worker가 되살리지 않는다. 단절 전에 시작된 저장은 완료될 수 있지만 ACK future는 그 연결에만 속한다.

## CLI 조회·정리

다음 명령은 중앙 업무 상태를 변경하지 않는다. Host는 기존 get_tasks(received)를 사용하며 새 MCP push/tool을 추가하지 않는다.

~~~powershell
devlink --config C:\work\profile\config.json inbox list --limit 20
devlink --config C:\work\profile\config.json inbox list --agent-id 34 --limit 100
devlink --config C:\work\profile\config.json status
devlink --config C:\work\profile\config.json stop
devlink --config C:\work\profile\config.json inbox clear
~~~

list는 디스크의 현재 원자 snapshot을 읽고 중앙 호출·디렉터리 생성·ACK·수락을 수행하지 않는다. 기본은 현재 serverUrl과 로컬 바인딩된 Agent다. 바인딩이 없는 legacy 프로필은 같은 서버의 기록을 표시한다. --agent-id는 로컬 표시 필터이며 인증 주체를 만들지 않는다. 최신 저장 순서부터 limit1~1024건의 server_url/message JSON을 표시해 requester_agent_id/task_id/prompt를 확인할 수 있다. C0는 JSON escape, DEL/C1/bidi/서식 문자는 표시 escape를 적용해 재파싱한 원문은 보존한다. 로컬 보관 데이터는 서버 최신 업무 상태가 아니다.

clear는 control.lock/broker.lock/inbox.lock 하에서 **해당 프로필의 모든 origin·Agent 기록과 dedup 이력**, private .inbox- 예약 임시 파일을 삭제한다. 실행 중인 브로커에서는 거부한다. 필요한 자료를 먼저 별도 private 보관해야 한다. 사용자/Agent/MCP 자격 파일과 중앙 업무·ACK·수락·완료는 변경하지 않는다. 손상된 원본이나 위험 경로를 자동 삭제하는 복구 명령으로 사용하지 않는다.

## 실패 진단과 검증

인증된 health의 선택적 inbox.rejectedEvents/lastError 및 CLI status에 내용 없는 STORE_FAILED/QUEUE_FULL 진단을 제공한다. 진단은 프로세스 수명 동안 유지되며 재시작 시 초기화된다. Connected는 디스크 수신 성공을 뜻하지 않는다. 저장 실패는 InboxFailed에서 자동 재접속하지 않는 upstream 정책을 유지한다.

- 단위/실소켓: 기존 schema1 보존·schema2 이력·event/task 충돌·원자 batch·정리/기록 상호 배제·손상 거부, 느린 기록 중 heartbeat와 무선행 ACK, 오래된 generation 무ACK, 16건 burst의 기본5초 예산을 회귀로 검증한다.
- 실제 CLI: start→수신→ACK→stop→새 instanceId start→새 event로 같은 task 재수신·ACK, 업무1건 유지와 event 이력 추가, 실행 중 clear 거부와 정지 후 정리, 제어 문자·로컬 Agent 필터를 확인한다.
- 실제 중앙 재현: loopback Spring/PostgreSQL을 준비하고 DEVLINK_TEST_CENTRAL_URL과 절대 DEVLINK_TEST_CLI를 지정해 저장소 루트에서 node rust/scripts/verify-task-inbox.mjs를 실행한다. 합성 사용자2명·실제 Rust 수신자·확인된 세션 presence로16건 ACK_RECEIVED, 재시작 후 새 업무 ACK, 실제 로컬 기록 실패의 무ACK/DELIVERY_UNKNOWN을 점검한다.
- 현재 재현 스크립트의 생성 경로는 Agent Bearer `POST /api/v1/tasks/{teamId}`다. 팀은 발급된 송신 Agent의 teamId를 사용한다. 이전 생성 URL만 제공하는 server snapshot은 지원하지 않으며, 최신 경로 변경의 실행 결과는 [작업 생성 호환 검증](business-relay.md#공식-생성-경로rejected-호환-검증)에 구분한다.
- 실제 중앙의 requestKey 재조회는 기존 업무를 재전송하지 않는다. 이를 wire 재전달 검증으로 세지 않으며, 같은/새 event 재전달과 재시작 dedup은 실소켓 fixture에서 별도로 검증한다.

~~~powershell
# rust 디렉터리
cargo +stable-x86_64-pc-windows-msvc test --locked --offline --workspace --all-targets -- --test-threads=1
cargo +stable-x86_64-pc-windows-msvc clippy --locked --offline --workspace --all-targets -- -D warnings
cargo +stable-x86_64-pc-windows-msvc fmt --all -- --check
~~~

실제 중앙 ACK 확인과 최종 TaskEvent 연결(-71), 전체 Rust 통합(-52), 사용자 수락 후 코드 수정·결과 회신(-66), 업무 중계/조회(-93/-94/-97)는 각 범위로 구분한다. 최신 중앙이 READY 사용자·팀을 공급하더라도 -92의 모든 인증 무효화·소유권·lease 수용을 이 스토리 테스트만으로 완료 처리하지 않는다.

## 2026-10-30 후속 보완·검증

기준 client는 `0516473` 이후 작업 트리, 검토 시 HEAD는 `3ee8fc7`, 실제 중앙은 `0e8c2b74e14ff056cdede24711e804647cd044df`다. 아래는 해당 실행·사용자 제공 보고의 기록이며 문서 통합 과정에서 재실행하지 않았다.

READY 이후 type=TASK_CREATED이지만 task_id가 빠진 입력을 조용히 무시하고 연결을 유지하는 결함을 소켓 회귀에서 재현했다. 수신 루프가 파싱 실패한 TASK_CREATED를 ProtocolViolation으로 명시적으로 종료하고 저장·ACK하지 않도록 보완했다. 관계없는 메시지는 PONG 대기 시간을 연장하지 않는다.

회귀에서는 동일 event/task를 재시작 전후 다시 보내도 정확한 event_id/task_id로 ACK하고 snapshot 바이트를 바꾸지 않는지 확인했다. 새 테스트는 READY 전 전달, 잘못된 팀·수신자, task_id 누락/문자열, event_id 누락/빈 값, requester=0, Long 초과 ID, prompt 공백/null, created_at 빈 값의 12개 입력을 검증한다.

| 실행 | 결과 |
| --- | --- |
| 전체 Rust workspace/all-targets | 168 passed / 0 failed / 2 ignored. 15개 실행 그룹이며 개별 회귀와 중복 합산하지 않음 |
| 실제 소켓 수신·ACK 회귀 | 위 전체 실행에 5건 포함. 단위 Inbox 9건·중앙 연결 10건·CLI 2건의 개별 선행 실행도 통과 |
| 포맷·Clippy | 전체 fmt 및 all-targets Clippy -D warnings, 스크립트 문법·diff 공백 검사 통과 |
| 실제 중앙·DB | 16건 burst + 재시작 후 새 업무 1건 ACK_RECEIVED. 실제 로컬 기록 실패의 ACK 0건·DELIVERY_UNKNOWN |

실제 중앙은 고정 소스 197개 파일의 바이트 일치를 확인해 Java 21로 격리 빌드했다. PostgreSQL 17.11의 전용 loopback DB/role devlink_relay_test를 사용하고 Flyway V1~V6·Hibernate validate를 유지했다. 합성 사용자 2명·임시 프로필만 사용했다. SQL 집계는 tasks=18, PENDING=18, ACK_RECEIVED=17, DELIVERY_UNKNOWN=1, ONLINE Agent=0, 성공 migration=6이었다.

같은 requestKey 재조회는 기존 작업 반환과 snapshot 무변경만 확인한다. 중앙이 메시지를 재전송한 증거로 세지 않으며 wire 재전달은 소켓 fixture가 담당한다. 기록 실패 시험 뒤 프로필을 복구·정리하고 자신이 시작한 중앙·DB를 종료했다. 런타임 DB 비밀번호도 제거했다. 한글 경로 initdb 인코딩 및 Windows 자식 출력 대기 문제는 검증 환경 준비 실패로 구분했고 성공 시험에 합산하지 않았다.

사용자 제공 Gemini 교차검토는 신규 결함 0건·추가 코드 수정 요구 없음이었다. 결과 기록 전에 변경 대상 5개와 관련 소스/증거를 포함한 manifest 19개 및 bundle 해시가 모두 일치했다. 원본 자료는 ignored target/story95의 gemini-review-bundle.md와 review-manifest.json에 보존한다. 이후 문서 정리로 문서 해시는 달라질 수 있으며 원본 검토를 새 문서의 재검토로 계산하지 않는다.

보고서의 역직렬화 대상은 ClientMessage가 아니라 ServerMessage로 정정한다. 제어 Ping/Pong은 JSON heartbeat 응답으로 인정하지 않는다. 현재 ping_id와 일치하고 기한 내 도착한 JSON PONG만 대기를 해제한다.

공개 가능한 실행 증거는 target/story95의 workspace-tests.log, clippy.log, actual-central.log, db-evidence.json, source-verification.json에 있다. private DB·서버 로그는 검토 묶음에 포함하지 않는다. 재현은 위 명령 및 저장소 루트의 node rust/scripts/verify-task-inbox.mjs를 사용한다.

Rust 수신함·ACK 후속 보완/검증/사용자 제공 교차검토까지의 결과다. 중앙 최종 전달 이력 전체 수용(-71), 오프라인 사전 거부(-67), 인증·세션 전체 수용(-92), MCP 최종 DTO(-97), 전체 통합(-52), 실제 Host 양방향(-66)·Ubuntu 실행은 별도다. 외부 이슈 상태나 커밋·푸시는 변경하지 않았다.
