# client·server·frontend E2E (1단계)

S15P31D108-179. [E2E 테스트 설계](https://www.notion.so/41235a4f3ca68100a873d878b8dbda7a)의 1단계 구현이다. 세 저장소를 실제 프로세스로 띄우고 Playwright Test(Node 24)로 브라우저 UI, CLI, MCP SDK를 한 테스트 안에서 구동한다. LLM Host는 MCP SDK 클라이언트로 대신한다.

## 실행

```powershell
# 처음 한 번: 고정 SHA checkout·빌드, PostgreSQL 16 휴대용 바이너리 준비(EDB zip, SHA-256 확인)
pwsh e2e/scripts/e2e-local.ps1 -InstallPostgres
# 연속 5회
pwsh e2e/scripts/e2e-local.ps1 -Runs 5
# 이미 설치된 PostgreSQL 16, 빌드 생략, 일부 시나리오
pwsh e2e/scripts/e2e-local.ps1 -PgBin C:\pg16\bin -SkipBuild -Grep S3
```

필요 도구: Node 24, JDK 21(`java`가 PATH에 없으면 `-Java`), Rust(`rust-toolchain.toml`), git. 스크립트는 `e2e/.stack/`(git 무시)에 server·frontend checkout, 빌드 산출물, 실행별 디렉터리 `runs/<runId>/`를 둔다.

## 구성

| 위치 | 역할 |
| --- | --- |
| `stack.lock.json` | server·frontend 커밋 SHA, PostgreSQL 16 Windows zip 주소·SHA-256. global setup은 checkout이 lock과 다르거나 jar·dist가 다른 커밋에서 빌드됐으면(스크립트가 남긴 빌드 마커로 판단) 실패한다. checkout 고정은 `E2E_STACK_MODE=latest`로 해제한다. client 작업 트리가 dirty면 영수증에 `clientDirty: true`로 남는다. |
| `src/stack/global-setup.ts` | 실행당 1회: 비공개 ACL 실행 디렉터리, MCP 런타임 사본, PostgreSQL 16, 정적 frontend + `/api` 프록시, central. 시작 전 server·frontend checkout이 깨끗한지, frontend에 `.env.local`·`.env.production.local`이 없는지, Java가 21인지 확인한다. java는 런처가 아닌 `java.home`의 실행 파일로 띄우고 종료할 때 프로세스 트리째 끝낸다. 결과는 `stack-receipt.json`(포트·PID·SHA·CLI와 MCP 해시, 비밀값 없음). |
| `src/stack/central.ts` | 기본 프로필, `-Duser.timezone=UTC`, `--server.address=127.0.0.1`, 임의 포트, 실행마다 생성한 `TASK_MESSAGE_CURSOR_SECRET`. 기본 프로필에 값이 없는 `devlink.websocket.allowed-origins`는 인자로 준다. |
| `src/stack/static-proxy.ts` | `dist/`를 제공하고 `/api`를 central로 넘긴다. `vite.config.ts`처럼 `Origin` 헤더를 지운다. |
| `src/client/actor.ts` | 액터별 `--config` 프로필, `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `git init`한 프로젝트, 가짜 Host(npm 배치의 `codex.cmd`/`claude.cmd`, 버전 응답만 함, 자동 설정 최소 지원 버전과 같은 Codex 0.166.0·Claude Code 2.1.317을 출력). 자식 환경에서 `DEVLINK_*`·`E2E_*` 제거. |
| `src/client/mcp.ts` | `packages/mcp-server/scripts/lib/mcp-stdio.mjs`(실제 중앙 검증 스크립트와 공유)로 MCP stdio 연결. |
| `src/pages/workspace.ts` | 역할·라벨 선택자 화면 객체. 새로고침 버튼을 누르며 `toPass`로 기다린다. |
| `specs/` | S1~S3 (`@p0`). S15 `find_agent` 실제 중앙 추천 중계와 후보에게 작업 전달·오프라인 후보 관찰(`@p1`, S15P31D108-188·-190). S16 3차 통합: setup part·roles → find_agent → 선택 → create_task → 수락·완료(`@p1`, S15P31D108-191). `E2E_REAL_HOSTS=1`이면 사용자 로그인을 쓰는 실제 Codex·Claude Code 턴도 실행한다(모델 비용 발생, [기록](../packages/mcp-server/docs/mvp3-integration.md)). |

## 시나리오

- **S1**: 웹에서 A 가입(자동 로그인 안 됨 확인)·로그인·팀 생성, B 가입·로그인·초대 코드 참여. 이어서 데스크톱 앱(frontend `desktop/setup-ipc.js`)과 같은 입력으로 `devlink --config X setup --non-interactive --input <json> --output jsonl --password-stdin --node-path --mcp-entry`를 실행한다(`agent.action=create`, `mcp.mode=apply`). JSONL이 모두 `schemaVersion: 1`, result가 마지막 한 건, result.exitCode와 프로세스 종료 코드 0, 이어 `setup status`의 `setupReady`·팀·도구·프로젝트 일치 순서로 판정한다. MCP 설정은 대상 파일에만 생기고, 브로커는 Connected, MCP `get_my_context`의 agentId가 웹 Agent 상세의 ID와 같다.
- **S2**: `register_agent` 후 `/agents/me` ONLINE, 두 브라우저에서 두 Agent "온라인"과 개요의 온라인 Agent 수 2. B `stop` 뒤 A 화면에서 B "오프라인", `start` + `register_agent`로 복구.
- **S3**: A `create_task` → `ACK_RECEIVED` → B 수신함·`get_tasks` → A의 상태 변경 거부(`BROKER_FORBIDDEN`) → B `IN_PROGRESS`와 같은 version 재호출 `TASK_VERSION_CONFLICT` → B `complete_task`(MODIFIED 파일 + diff) → A `get_updates`의 `TASK_COMPLETED`·`get_task_result` COMPLETED. A·B 웹 대화 화면에서 상태, 결과 텍스트, 파일 경로·diff, 이벤트 기록을 확인한다. 역방향 B→A는 MCP 단계만 반복한다.

S2·S3은 `seedTeam` 픽스처가 REST로 가입·팀을 만들고 같은 데스크톱 setup 경로로 Agent를 연결한다. 이메일은 실행·시나리오·반복마다 새로 만들어 액터마다 다른 central 사용자를 쓴다. setup은 기계 단위 브로커 소유 잠금(`%LOCALAPPDATA%\DevLink\broker-owners\`, central 사용자별 공유 잠금과 Agent별 잠금)을 잡기 때문이다.

## 비밀정보

- 비밀번호는 실행마다 생성해 메모리와 stdin으로만 넘긴다. DB 비밀번호는 `initdb --pwfile`에 필요한 순간에만 비공개 임시 파일로 쓰고 바로 지운다. DB 비밀번호와 cursor secret은 그것을 써야 하는 central(`DB_PASSWORD`, `TASK_MESSAGE_CURSOR_SECRET`)과 `createdb`(`PGPASSWORD`)의 환경 변수로만 넘기고, 테스트 워커·브라우저·CLI·MCP 자식 프로세스에는 넘기지 않는다.
- 각 테스트 끝에 생성·읽은 비밀번호, 초대 코드, Agent 토큰, 로컬 토큰, CLI·브라우저 세션 쿠키가 실행 로그, 액터 디렉터리(setup이 쓴 `.mcp.json`·`.codex/config.toml` 포함), central 로그, 영수증, 테스트 결과의 텍스트 파일(`.log/.json/.txt/.md/.jsonl/.toml`)에 없는지 검사한다. 실행이 끝나면 러너가 DB 비밀번호·cursor secret으로 같은 검사를 한다. 있으면 실패한다.
- 액터 프로필(`profile/`, CLI가 저장한 토큰 포함)은 테스트가 끝나면 지우고, 지우지 못하면 테스트를 실패시킨다.
- Playwright는 `fill()`·`type()`의 인자를 단계 제목(`Fill "<값>"`)으로 HTML 리포트와 오류 로그에 남긴다. 그래서 비밀번호와 초대 코드는 `fillSecret()`(값을 단계 제목에 넣지 않는 `evaluate`)으로 입력한다. trace는 로그인 요청 본문을 담고 zip이라 검사할 수 없어 기본으로 끈다. 디버깅할 때만 `E2E_TRACE=1`로 켜고 확인 후 `e2e/test-results`, `e2e/playwright-report`를 지운다. 실패 시 남는 스크린샷·영상에서 비밀번호 칸은 가려지지만, "팀을 만들었습니다" 대화상자의 초대 코드는 보일 수 있다(일회용 central의 값).
- 실패하면 Playwright가 `error-context.md`에 페이지 aria snapshot을 넣고, 이 snapshot은 입력 칸 값(비밀번호 칸 포함)을 그대로 담는다. 실패한 locator 단언은 자체 snapshot을 붙이고, 그 밖의 실패는 컨텍스트를 닫을 때 snapshot을 뜬다. 대응은 세 가지다.
  - 비밀번호·초대 코드 칸은 제출 직후 `clearSecrets()`로 비운다. 제출 처리기는 이미 값을 읽은 뒤라 동작에 영향이 없고, 로그인이 실패해도 칸에 값이 남지 않는다.
  - `playwright.config.ts`에서 `PLAYWRIGHT_NO_COPY_PROMPT=1`로 컨텍스트 종료 시 snapshot을 끈다(locator 단언의 snapshot은 이 값으로 꺼지지 않는다).
  - `error-context.md`는 픽스처의 평문 검사보다 늦게 쓰이므로, 각 테스트는 비밀값의 SHA-256만 `runs/<runId>/secret-digests/`에 남기고 러너 teardown이 `test-results`·로그·액터 디렉터리의 토큰을 해시로 대조한다. 발견하면 실행을 실패시킨다. HTML 리포트는 같은 첨부 파일을 복사하므로 `test-results` 검사로 함께 확인된다.
  - 감지돼 실행이 실패하면 해당 파일은 그대로 남고, 리포트에도 복사된다. 원인을 확인한 뒤 `e2e/test-results`, `e2e/playwright-report`, 해당 `runs/<runId>`를 지운다.

## 알려진 한계

- Windows에서 `~/.devlink-mcp-config/`(setup의 MCP 적용 잠금·백업)와 `%LOCALAPPDATA%\DevLink\broker-owners\`는 격리되지 않는다. 앞의 것은 테스트가 끝날 때 대상 경로의 디렉터리를 지우고, 뒤의 것은 사용자·Agent별 빈 잠금 파일(`<digest>.lock`, `<digest>.agent-<id>.lock`)이 남는다.
- Ctrl+C나 워커 비정상 종료로 테스트 정리가 건너뛰어지면 setup이 띄운 브로커와 `runs/<runId>/actors/*/profile`(토큰 포함)이 남을 수 있다. `devlink --config <profile>\config.json stop`으로 멈추고 해당 `runs/<runId>`를 지운다.
- S6(오프라인 수신자)는 정책 상의 중이라 포함하지 않는다. Linux CI 워크플로우(`.github/workflows/e2e.yml`)는 후속이다.
