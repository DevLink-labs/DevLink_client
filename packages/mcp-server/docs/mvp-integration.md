# MVP 실제 통합 검증 — S15P31D108-66

## 과거 판정 (2026-10-28, Asia/Seoul)

> 아래 판정과 표는 2026-10-28 당시 기록이다. 최신 관측은 아래 "수용 기준과 증거 양식"의 2026-11-06 표와 [실제 두 Host 수용 검증](central-host-acceptance.md#2026-11-06-재검증-develop-병합-후)을 따른다.

**실제 MVP 수용 기준은 미검증이며 스토리를 완료로 처리하지 않는다.** 별도 통합 환경이 없어 이번 변경은 준비 상태 검사, 실제 Rust 재현, 후속 검증 절차를 제공한다. 기존 가짜 브로커 기반 Host 테스트(-64·-65)는 실제 중앙 서버·두 사용자 프로젝트의 결과를 대신하지 않는다.

| 대상 | 확인한 소스/실행 결과 | 남은 조건·담당 스토리 |
| --- | --- | --- |
| client develop | `63f61fc4c9de1d94c39e2aa877029dfa0d6fc3c7` (PR #12 포함) | Rust에 `/health`, `/shutdown`만 구현. 세션 4개·업무 7개 경로 연동은 -50 최준혁과 계약 확인 필요 |
| 실제 Rust 실행 | 개인 임시 설정, 합성 토큰, 의도적으로 연결 불가인 로컬 중앙 주소. MCP 자격 파일 검증 성공, health 200, 필수 경로 OPTIONS **11개 모두 404**, 검사 종료 2 | 중앙 인증·세션·업무 계약을 구현한 빌드 필요. 이 실행은 외부 중앙 환경을 사용하지 않음 |
| server develop | `ffb6930e8eb68240709d9ff6e292263aaed9bcdd`의 Controller는 health만 존재. AgentTokenVerifier는 인터페이스이며 SecurityConfig는 구현 부재 시 인증 거부 | 등록·조회 -84/-85/-86 박성수, 인증 -49 최보경. 배포 상태는 별도 환경 부재로 확인 불가 |
| WebSocket | Rust는 연결 후 `auth` 전송→`auth_ok` 기대. 중앙은 HTTP Agent 인증→`BROKER_READY`, `HEARTBEAT` JSON 사용 | -47/-50 최준혁, -49/-56 최보경의 실제 연결 규격 조정 |
| 업무 도메인 | 중앙 라우팅은 확인 당시 미병합 server PR #9. 생성·상태·결과 API는 develop에 미반영 | -53/-54 최보경, -67/-68/-69 이정민, -75/-76/-77 지상근 |
| 사용자 환경 | 현재 PC에 기본 중앙 config, DEVLINK_TOKEN, DEVLINK_BROKER_FILE 없음. 두 사용자·프로젝트·테스트 자격 정보 미제공 | 실제 계정·팀·Agent·자격 파일·프로젝트 준비 후 아래 절차 수행 |

근거: [client Rust 라우터](https://github.com/SSAFY-URAYAHA/DevLink_client/blob/63f61fc4c9de1d94c39e2aa877029dfa0d6fc3c7/rust/crates/devlink-broker/src/broker.rs), [Rust 중앙 프로토콜](https://github.com/SSAFY-URAYAHA/DevLink_client/blob/63f61fc4c9de1d94c39e2aa877029dfa0d6fc3c7/rust/crates/devlink-broker/src/protocol.rs), [server 소스](https://github.com/SSAFY-URAYAHA/DevLink_server/tree/ffb6930e8eb68240709d9ff6e292263aaed9bcdd), [server 라우팅 PR #9](https://github.com/SSAFY-URAYAHA/DevLink_server/pull/9), [Jira -66](https://ssafy.atlassian.net/browse/S15P31D108-66).

소스 확인은 위 커밋에 한정된다. 다른 브랜치·배포 환경의 준비 완료를 뜻하지 않는다. 담당 스토리의 Jira 완료 표기도 배포·상호 호환을 보장하지 않는다.

## 준비 상태 검사

저장소 루트, Node.js 24 이상에서 실행한다. 기존 MCP 소비자와 동일한 자격 파일 스키마·개인 권한·루프백 URL 검사를 재사용한다.

```powershell
npm --prefix packages/mcp-server ci
npm --prefix packages/mcp-server run integration:check -- --broker-file 'C:\absolute\profile\broker.json'
# 또는 실제 Host의 DEVLINK_BROKER_FILE을 설정한 셸에서
npm --prefix packages/mcp-server run integration:check
```

검사기는 `GET /health`로 서비스·버전·필수 health 필드를 확인한 후 11개 경로에 `OPTIONS`만 전송한다. 토큰은 지정된 `http://127.0.0.1:포트`에만 전송하고, 프록시·리다이렉트를 따르지 않는다. 세션 생성·복구·bind, 업무 작성·수락·종료, 브로커 종료를 호출하지 않는다. health가 확인되지 않으면 추가 경로를 조회하지 않는다.

각 HTTP 요청의 절대 제한은 기본 2000ms이며 `--timeout-ms 1..10000`으로 지정한다. 순차 12회이므로 기본 HTTP 검사 상한은 약 24초이고 파일 권한 검사 시간이 추가된다. 응답은 각 16KiB 이내로 제한한다. 공유 가능한 stdout JSON에는 토큰, 자격 파일 경로, URL, 원문 응답·오류를 싣지 않는다.

| 종료 코드 | 의미 |
| --- | --- |
| 1 | 인자 오류·예기치 않은 검사 실패 또는 격리 Rust 재현 도구의 실행·정리 실패 |
| 2 | `blocked`: 자격 파일·health·중앙 연결·경로 검사에서 차단 사유 관측 |
| 3 | `unverified`: 뚜렷한 차단 사유 미관측. 여전히 실제 검증 필요 |

도움말만 0으로 끝난다. 준비 상태 검사는 **항상 `actualMvpVerified: false`**이며 성공 판정을 제공하지 않는다. 404는 해당 OPTIONS 요청이 실패했다는 증거이고 소스 확인과 함께 해석해야 한다. 2xx·405는 공통 OPTIONS 처리나 메서드 분기일 수 있다. 401·403은 권한 거부이며 어느 응답도 실제 POST/DELETE 계약·중앙 DB·ACK·권한 정책을 증명하지 않는다. 경로를 제공해도 OPTIONS를 막는 환경은 소유자와 확인하고 실제 시나리오로 판정한다.

## 최신 Rust 누락 경로 재현

Windows MSVC 빌드 기준이다. GNU 기본 타깃에서 `dlltool`이 없으면 아래 설치된 MSVC 툴체인을 명시한다.

```powershell
cargo +stable-x86_64-pc-windows-msvc build --manifest-path rust/Cargo.toml --locked -p devlink-cli
$integrationCli = (Resolve-Path 'rust/target/debug/devlink.exe').Path
npm --prefix packages/mcp-server run integration:rust -- --cli $integrationCli
# 현재 develop에서는 health 200, 11개 OPTIONS 404, 종료 코드 2 예상
```

재현 도구는 사용자 전용 임시 디렉터리에 설정을 만들고 지정한 실제 CLI의 foreground broker를 실행한다. 상속된 `DEVLINK_*` 환경변수를 대소문자 구분 없이 제거하고 DEVLINK_TOKEN만 매번 합성 값으로 지정한다. 중앙 주소는 도구가 소유한 루프백 503 서버다. 본인 설정·실제 중앙 토큰은 사용하지 않는다. Rust가 발행한 자격 파일의 개인 권한·스키마와 health를 확인할 때까지 기다린 뒤 검사한다. 이후 소유 자식 프로세스·로컬 서버·임시 파일을 정리한다. 정리 실패 시 종료 1이며 아직 실행 중일 수 있는 소유 프로세스의 설정은 보존한다.

**종료 2만으로 재현 성공으로 판단하지 않는다.** 현재 기준 JSON에서 `credentials: "valid"`, `health.httpStatus: 200`, 11개 경로의 `observation: "NOT_FOUND"`와 `httpStatus: 404`를 함께 확인한다. 이 명령은 실제 Rust의 로컬 API 준비 상태 재현이며 중앙 통합 시험은 아니다. 향후 API가 추가되면 관측 JSON도 바뀌지만 중앙은 의도적으로 연결 불가이므로 차단 판정은 유지된다.

npm 셸 wrapper의 인자 전달 문제가 있으면 먼저 빌드한 뒤 아래처럼 Node를 직접 실행한다.

```powershell
node packages/mcp-server/scripts/integration-readiness.mjs --broker-file 'C:\absolute\profile\broker.json'
node packages/mcp-server/scripts/verify-rust-readiness.mjs --cli $integrationCli
```

## 실제 환경 준비 후 재개 절차

1. 중앙 배포 커밋, Rust 빌드 커밋, MCP 버전, Host 버전, 테스트 일시(KST)를 고정한다. HTTP 인증·WebSocket 이벤트·ACK·상태/오류 DTO를 담당자와 맞추고 위 소스 차이를 해소한다.
2. 같은 팀의 서로 다른 두 사용자 A/B, 각 Agent, 각자의 CLI 발행 개인 자격 파일, 별도 로컬 프로젝트 A/B를 준비한다. 교차 팀 거부용 사용자 C, 존재하지 않는 Agent ID, offline Agent, 전달 실패·지연·중복 요청을 안전하게 유도할 테스트 환경도 준비한다. 자격 파일 원문은 문서나 프롬프트에 붙여 넣지 않는다.
3. 각 사용자 환경에서 준비 상태 JSON을 수집한다. 건강한 중앙 연결과 실제 세션/업무 API의 통합 확인이 선행되어야 한다. 준비 상태 검사 자체로 수용 기준을 통과시키지 않는다.
4. 각 Host에 [수락·결과 등록 지침](agent-workflow.md)과 실제 개인 프로필을 적용한다. 사용자가 선택한 수신자를 `list_agents`로 확인하고, 업무 요청에 실제 프로젝트의 작은 변경(예: 함수 추가와 해당 테스트)을 명시한다. MCP는 직접 파일을 고치지 않으며 수신 Host가 명시적 사용자 수락 후 수행한다.
5. A→B에서 `create_task` 응답의 requestId/requestKey/taskId와 저장·전달 결과를 기록한다. B의 수신 목록/detail에 PENDING이 보이며 수락 전 파일 diff가 없는지 확인한다. B 사용자가 수락한 뒤 `update_task_status(IN_PROGRESS)`를 호출하고 실제 프로젝트에서 편집·테스트한 후 `complete_task`에 실제 변경 파일·요약·검증 결과를 등록한다. A에서 `get_tasks`, `get_task_result`로 동일 taskId의 COMPLETED와 실제 diff/테스트 결과가 일치하는지 확인한다.
6. B→A도 별도 taskId로 수행한다. 각 프로젝트의 변경 전후 커밋 또는 diff, 실행 명령·종료 코드·관측값을 보존한다. 미실행 테스트를 통과로 기록하지 않는다. 실패·부분 수정 시 FAILED 전이에 failureReason/partiallyModified를 기록하고 발신자 조회까지 확인한다.
7. 아래 부정 시나리오는 전용 테스트 데이터·제어 가능한 지연/전달 장애로 실행한다. MCP `create_task`는 선택 requestKey를 받는다(S15P31D108-165). 같은 키·같은 인자 재호출은 같은 taskId와 `replayed: true`를, 같은 키·다른 인자는 REQUEST_KEY_CONFLICT를 확인한다. `get_tasks`는 requestId/requestKey 검색을 지원하지 않으므로 중앙 저장 건수는 DB 확인 수단으로 대조한다. requestKey 없이 재시도한 Host 호출은 새 요청이므로 중복 검증으로 세지 않는다.

## 수용 기준과 증거 양식

실제 실행 후에만 PASS/FAIL로 바꾸고 원문 토큰·개인 정보는 제거한다. 정상 두 방향은 각각 독립된 증거를 남긴다.

2026-11-06 기준 관측은 아래와 같다. 근거는 [실제 두 Host 수용 검증](central-host-acceptance.md#2026-11-06-재검증-develop-병합-후)이고, 실행 조합은 client develop `dfc28c7`, 중앙 develop `34b2c15`, PostgreSQL 17.11이다.

| ID | 관측 |
| --- | --- |
| AC1-A / AC1-B | PASS. Codex→Claude task 1, Claude→Codex task 2. 실제 diff·결과 조회 일치는 확인했다. requestId·requestKey는 실행기 report에 남지 않는다 |
| AC3 | 부분 확인. 판정기는 두 방향의 PENDING 조회 → 명시적 IN_PROGRESS → COMPLETED 호출 순서만 확인했다. 수락 전에 로컬 파일이 바뀌지 않았는지와 각 시점의 파일 diff는 확인하지 않았다. FAILED 종료 경로는 AC2와 함께 미실행 |
| AC2 | NOT_RUN |
| AC4·AC5·AC6 | 두 Host로는 NOT_RUN. 근거는 중앙 `d06d239` 기준 통합 시험(-67 등, 575건)뿐이다 |
| AC7 | NOT_RUN |

아래 표는 각 ID의 정의다.

| ID | 재현·예상 결과 | 필수 증거 |
| --- | --- | --- |
| AC1-A / AC1-B | A→B / B→A 요청→명시적 수락→실제 편집·테스트→완료→발신자 조회 | 각 방향 requestId/requestKey/taskId, 실제 diff, resultText(요약·검증)·taskFiles(유형·diff)과 조회 일치 |
| AC2 | 부분 파일 수정 후 테스트 실패→FAILED 등록→발신자 조회 | failureReason, partiallyModified, 실제 남은 diff와 실패 명령/종료 코드 |
| AC3 | 수락 전 PENDING, 수락 후 IN_PROGRESS, 종료 후 COMPLETED 또는 FAILED | 각 시점 조회·파일 diff, 수락 시각. 미수락 동안 로컬 변경 없음 |
| AC4 | 교차 팀·존재하지 않는 수신자·offline 수신자에 각각 요청 | 정책 오류, 신규 task/이벤트/전달 없음(DB·중앙 관측). 동일 팀 정상 기준과 비교 |
| AC5 | 저장 후 전달만 실패하도록 제어 | saved=true 및 전달 실패, 동일 taskId PENDING 유지, 지정 관찰 구간 동안 자동 재전달 없음 |
| AC6-D | 동일 unique key 동시/반복 전송 | 담당자 통합 시험에서 동일 taskId, DB 작업 1건, 중복 전달 없음 |
| AC6-T | 처리/응답 지연으로 클라이언트 제한 시간 초과 유도 후 조회 | 자동 FAILED 전이·새 키 재요청 없음, 이후 실제 상태와 taskId(알려진 경우) 보존 |
| AC7 | 제3자가 동일 환경에서 재실행 | 버전·설정의 비밀 제외 요약, 명령, 예상/관측, 증거 링크, 미해결 결함 담당/스토리 |

각 실행 기록은 다음 형태로 Notion의 작성일 문서에 남긴다.

```text
scenario / direction / KST:
client commit / Rust commit / central deployment commit / Host versions:
test user aliases / team aliases / agent aliases / project baseline commits:
reproduction steps and commands:
requestId / requestKey / taskId (unknown 값은 unknown 유지):
expected / observed / PASS | FAIL | BLOCKED | NOT_RUN:
file diff / test command + exit / sender query evidence:
delivery observation interval / controlled fault settings:
unresolved issue / owner / linked story / next action:
```

저장·전달·업무 상태를 각각 기록하고, 결과 불명이나 timeout을 FAILED로 바꾸지 않는다. 자동 재전송이나 수락 전 편집이 관측되면 실패로 기록한다. 실제 통합 수용 기준 7개가 모두 충족되어야 -66 완료를 판단할 수 있다.
