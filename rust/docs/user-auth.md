# CLI 사용자 로그인과 세션

간편설정의 세션 재사용·필요 시 로그인 입력과 장애 분류는 [setup 작업 04](setup.md#작업-04-로그인-상태-확인과-단계-통합)를 참고한다. 개별 auth 명령은 아래의 기존 호출 형식을 유지한다.

S15P31D108-89. 중앙 `a4858d6`의 세션 인증·CSRF 계약을 소비한다. 회원가입은 기존 중앙 API를 사용하며 이 스토리에 가입 명령을 추가하지 않는다.

```powershell
devlink init
devlink config set serverUrl wss://your-central.example
devlink auth login --email user@example.com
devlink auth status
devlink auth logout
```

로그인은 터미널에서 비밀번호를 숨김 입력한다. 자동화에서는 비밀번호를 명령행 인수나 환경변수 대신 표준입력 한 줄로 보내고 `--password-stdin`을 사용한다. 비밀번호의 공백을 자르지 않으며 LF/CRLF 줄 끝만 제거한다. 비밀번호 인수와 수동 userId/teamId 입력은 제공하지 않는다. HTTPS를 사용하며 HTTP는 loopback 개발 서버에만 허용한다.

1. 로그인 전 GET `/api/v1/auth/csrf`로 쿠키와 CSRF 토큰을 얻는다.
2. 같은 cookie jar와 CSRF 헤더로 POST `/api/v1/auth/login`을 호출한다.
3. 교체된 세션 쿠키로 GET `/api/v1/auth/users/me`를 호출해 신원을 확인하고 CSRF 토큰을 다시 얻는다.
4. 이후 세션 명령은 `/users/me`로 실제 신원을 재검증한다. 변경 요청에는 새 CSRF 토큰을 사용한다. redirect·HTTP 자동 재시도·프록시는 사용하지 않는다.

사용자 세션은 설정 디렉터리의 `.devlink-runtime/user-session.json`에 별도로 저장한다. Windows 사용자 전용 ACL, Unix 0600/0700, 안전한 경로 검사와 같은 디렉터리의 임시 파일→flush→원자 교체를 사용한다. 16KiB 상한과 account.lock으로 잘못된 파일·동시 갱신을 거부한다. 토큰·비밀번호·서버 오류 본문은 터미널에 출력하지 않고 사용자 정보의 제어·bidi 문자도 이스케이프한다. 저장된 세션은 정확한 중앙 origin에 묶인다.

MCP `broker.json`의 localToken, CLI 제어용 runtime.json의 controlToken, 중앙 Agent 토큰과 이 사용자 쿠키는 서로 다른 자격이다. 사용자 세션/비밀번호를 MCP Host 환경변수에 넣지 않는다.

`auth status`는 authenticated/logged_out/expired를 구분한다. 서버가 만료/폐기된 세션에 401을 반환하면 쿠키를 삭제한 expired 스냅샷을 저장하고 재인증을 안내한다. 이후 사용자 세션이 필요한 호출은 로그인 전 차단한다. 중앙 단절을 세션 만료로 판단하지 않으며, 잘못된 비밀번호의 로그인 실패는 기존 정상 로컬 세션을 덮어쓰지 않는다.

중앙 세션의 기본 유휴 만료는30분이며 서버 설정을 따른다. serverUrl 변경 후 status는 origin_mismatch, 안전하게 읽었지만 JSON/schema가 손상된 파일은 invalid_local_session으로 표시한다. 이 상태에서 auth login은 기존 쿠키를 새 origin에 보내지 않고 검증된 새 로그인으로만 파일을 교체한다. auth logout은 네트워크 없이 로컬 자격을 지우고 원격 무효화가 수행되지 않았음을 안내한다. 경로/ACL/크기 검증 실패는 복구용 덮어쓰기를 허용하지 않는다. 다른 계정으로 로그인해도 이전 서버 세션을 자동 종료하지 않으며 이전 세션은 해당 서버의 유휴 만료를 따른다. 프록시가 필수인 네트워크는 현재 지원하지 않는다.

로그아웃은 중앙 세션 무효화를 요청하고 로컬 쿠키를 지운다. 중앙 통신 실패 시에도 로컬 자격을 지우지만 원격 로그아웃이 확인되지 않았음을 실패 종료로 알린다. 별도의 Agent 토큰 폐기·브로커 종료·MCP 토큰 회전은 수행하지 않는다.

검증: `cargo test -p devlink-cli --test cli_auth -- --test-threads=1`은 실제 CLI 프로세스와 계약 fixture로 쿠키/CSRF 회전·신원·실패·만료·로그아웃·origin 격리·비밀 비노출을 확인한다. 실제 중앙/DB 결과와 Opus 검토·최종 커밋은 별도 검증 기록에 명시한다. fixture 통과를 실제 중앙 연동 완료로 간주하지 않는다.

## 2026-10-29 실제 중앙 검증

중앙 소스 `a4858d6a9386b811eaa541268464969b67ea3ce5`를 수정 없이 빌드해 loopback에서 PostgreSQL17.11의 새 클러스터에 연결했다. Docker Desktop의 자체 시작 오류 때문에 공식 EDB Windows 바이너리를 작업 폴더에서만 실행했으며 전역 DB/서비스는 변경하지 않았다. 테스트 계정만 생성했다.

`scripts/verify-user-auth.mjs`는 `DEVLINK_TEST_CENTRAL_URL`과 절대 실행 파일 경로 `DEVLINK_TEST_CLI`를 받아 기존 가입 API로 합성 계정을 준비하고 실제 CLI를 기동한다. 로그인·/users/me 확인·잘못된 비밀번호에서 기존 세션 보존·원격 로그아웃 뒤401/로컬 만료 정리·CLI 로그아웃을 검증해 종료0을 확인했다. 테스트 중앙의 유휴 만료를1분으로 설정하고 `DEVLINK_TEST_IDLE_EXPIRY_MS=65000`으로 실제 시간 만료도 확인했다. 운영 서버의 기본30분 설정을 바꾸지 않는다. 비밀번호·쿠키는 인수/결과에 넣지 않고 임시 자격 디렉터리는 정리한다.

Windows MSVC의 인증 CLI fixture8건, 타입 검사와 전체 Clippy 경고0을 확인했다. 새 origin/손상된 JSON에서 복구할 수 없던 Opus P2를 수정하고2개 loopback origin의 복구·신원 변경·5xx 원본 보존·초기 로그인 실패 회귀를 추가했다. 최종 전체 회귀와 Opus 재검토 결과는 해당 PR 및 날짜별 Notion 기록에 남긴다. 업무용 MCP 세션·작업 왕복 완료를 주장하지 않는다.
