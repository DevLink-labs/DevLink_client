# 검증 요약과 재현

정리일: 2026-10-30 (Asia/Seoul). 아래는 기존 실행 기록의 요약이다. 이번 문서 정리에서 기능 테스트·중앙 서버·Gemini/Claude를 다시 실행하지 않았다. 날짜가 같아도 다른 코드·서버 조합의 결과를 합산하지 않는다.

## 최근 검증

| 대상 | 코드·중앙 기준 | 확인한 결과와 범위 |
| --- | --- | --- |
| 수신함·ACK 후속 보완(-95) | client `0516473` 이후 작업 트리, 검토 HEAD `3ee8fc7`; 중앙 `0e8c2b7` | Rust 전체 168 passed / 0 failed / 2 ignored. 실제 소켓 5건 포함. Clippy `-D warnings`·fmt 통과. 실제 중앙 burst 16건 + 재시작 후 1건 ACK, 기록 실패 ACK 0건. Flyway V1~V6·schema validate 유지 |
| 상태·완료·목록·결과(-94) | client `8199ba7` 대비 구현; 중앙 `a5c7987` | 당시 전체 Rust 164건 통과·2건 제외, 이후 보강한 중계 7건 통과. 실제 Rust→Spring→PostgreSQL 왕복. 임시 DB ddl-auto=update·Flyway 비활성화로 운영 migration 증거는 아님 |
| 공식 생성 경로·REJECTED 호환(-93/#26) | client `8199ba7`→`4188973`; 중앙 `a5c7987` | 당시 Rust 159건·HTTP 회귀 20건·MCP 100건 통과. 실제 거절 후 같은 키 재접수·taskId 유지·수신함 ACK 검증. Flyway V1~V4·schema validate 유지. 이후 충돌 통합본에 이전 실행 결과를 재귀속하지 않음 |
| 중앙 READY·로컬 세션 | client `d4c3754`; 중앙 `02de434` | 실제 READY 신원·SDK 세션·소유권·재시작·90초 lease·SQL presence·토큰 교체 거부 확인. 실제 Host 전체 업무 왕복과 구분 |

세부 동작·교차검토·남은 조건은 [수신함](task-inbox.md), [업무 조회·변경](task-operations.md), [작업 생성](business-relay.md), [세션 검증 이력](archive/session-verification-2026-10-29-30.md)을 따른다. 과거 단위·SDK·MCP·CLI 실행 이력은 [초기 검증 기록](archive/verification-2026-10-27-30.md)에 보존한다.

## 로컬 회귀 명령

Windows 검증은 stable MSVC를 사용했다. 의존성 캐시가 없으면 `--offline`을 제외한다. 로컬 TCP·파일 ACL·자식 프로세스가 필요한 테스트는 실행 환경의 권한을 확인한다.

```powershell
# 저장소의 rust 디렉터리
cargo test --locked --offline --workspace --all-targets -- --test-threads=1
cargo clippy --locked --offline --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

기본 전체 실행에는 실제 45초 heartbeat 유지 등 시간 기반 테스트가 포함된다. ignored 2건은 Node·MCP 빌드와 실제 lease 대기가 필요한 opt-in 시험이며 기본 회귀 통과 수에 포함하지 않는다. 실행법은 [Rust README](../README.md)를 참고한다.

```powershell
# 저장소의 packages/mcp-server 디렉터리
npm.cmd test

# 저장소 루트: 실제 중앙을 쓰는 스크립트의 문법만 확인
node --check rust/scripts/verify-business-relay.mjs
node --check rust/scripts/verify-task-operations.mjs
node --check rust/scripts/verify-task-inbox.mjs
```

## 실제 중앙·DB 재현

기능별 스크립트와 환경변수는 각 기준 문서에 둔다. `DEVLINK_TEST_CENTRAL_URL`은 격리 loopback 서버, `DEVLINK_TEST_CLI`는 현재 빌드의 절대 경로다. SQL 검사가 필요한 중계 스크립트는 전용 DB/role과 `DEVLINK_TEST_PSQL`·`DEVLINK_TEST_DB_PORT`도 사용한다.

전용 합성 계정·프로필·DB로 시험하고, 자신이 시작한 프로세스만 종료한다. 운영 DB·사용자 Host 설정을 시험 대상으로 쓰지 않는다. 토큰·쿠키·비밀번호와 private DB/로그는 검토 자료에 포함하지 않는다.

## 결과를 읽는 기준

- 합성 중앙 fixture, 실제 Rust 소켓/CLI, 실제 중앙·DB, 실제 Host 시험을 구분한다.
- 동일 requestKey 재조회는 서버가 메시지를 재전송했다는 증거가 아니다. wire 중복·재전달은 소켓 fixture에서 확인한다.
- ACK는 로컬 저장 확인이다. 수락·편집·완료 및 업무 exactly-once 실행의 증거가 아니다.
- Windows 통과로 Ubuntu·CI·운영 배포·전체 Host 왕복까지 완료 처리하지 않는다.
- 이 문서의 테스트 수는 각 실행 당시 기록이다. 이후 문서 정리나 다른 브랜치 병합만으로 새 실행 결과가 되지 않는다.
