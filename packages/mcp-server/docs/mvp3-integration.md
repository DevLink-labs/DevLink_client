# 3차 MVP 통합 검증 기록 (S15P31D108-191)

3차 MVP(담당 Agent 자동 탐색·지정)의 client 스토리를 실제 중앙과 두 Host로 교차 검증한 기록이다. 2차의 S15P31D108-144와 같은 방식으로 증거 수준을 나눈다. **선행 스토리 일부가 아직 없어 이 스토리는 부분 검증 상태다.** 실행하지 못한 항목은 아래에 미검증으로 남긴다.

## 실행 환경 (2026-11-09)

- **client**: 브랜치 `feature/ai/mvp3-integration`. develop `7694e03`(SET-3-01 PR #101 포함)에 S15P31D108-188~190을 합친 상태다.
- **server**: develop `d945f8b`. E2E 고정 커밋을 `aecdcc3`에서 올렸다. 이유는 두 가지다.
  - `aecdcc3`의 Agent 생성 요청 DTO(`CreateAgentRequest`)에는 `roles`가 없다.
  - `GitIdentityController`도 없다.
- **로컬 환경**: Windows 11, PostgreSQL 16.15, JDK 21, Node 24.
- **실제 Host**: 버전은 실행 때 annotation에 남긴다.
  - Codex 0.167.0(`codex exec`): CLI 기본 모델을 쓰며, JSONL에 모델 ID가 없다.
  - Claude Code 2.1.319(`claude -p`): `claude-opus-5-5`/high.
- **Host 격리**:
  - 두 Host 모두 사용자 로그인을 쓴다. MCP 설정은 DevLink 하나만 둔다(`--strict-mcp-config`, `--ignore-user-config`).
  - 저장소 밖 임시 디렉터리에서 실행한다. 이 저장소의 `AGENTS.md`와 프로젝트 설정이 문맥에 들어가지 않게 하기 위해서다.
  - Codex는 `project_doc_max_bytes=0`으로, Claude Code는 `--setting-sources project`로 지침·사용자 설정 로드를 줄였다. 각 Host의 전역 지침까지 모두 막았는지는 확인하지 않았다. 사용자 settings의 `env`·`apiKeyHelper`로 인증하는 Claude Code 환경에서는 `--setting-sources project` 때문에 로그인이 빠질 수 있다(이번 환경은 OAuth 로그인이라 유지됐다).

## 재실행

```text
pwsh e2e/scripts/e2e-local.ps1 -Grep "S16"                 # SDK 단계
$env:E2E_REAL_HOSTS = '1'; pwsh e2e/scripts/e2e-local.ps1 -SkipBuild -Grep "S16"   # 실제 Host 단계 포함
```

시나리오는 [`e2e/specs/p1-s16-mvp3-integration.spec.ts`](../../../e2e/specs/p1-s16-mvp3-integration.spec.ts)다. 실제 Host 단계는 모델 호출 비용이 들어 opt-in이고, MCP 프로세스 추적 때문에 Windows에서만 실행한다. transcript는 실행 디렉터리의 `real-hosts/`에 남고, 측정값·Host 출력은 Playwright annotation에 남는다.

## 증거 수준

"실제 Host"에서 자동으로 검사하는 것은 도구 이벤트(호출 순서·인자·결과)뿐이다. 모델이 사용자에게 쓴 표시 문구는 annotation으로 남기고 사람이 확인했다.

| 항목 | fake | 실제 Rust | 실제 중앙 | 실제 Host |
| --- | --- | --- | --- | --- |
| SET-3-01 part·roles를 setup으로 등록하고 중앙에 저장 | – | 통과 | 통과 | – (CLI 단계) |
| SET-3-02 담당 정보 수정(-183) | – | 미구현 | 미검증 | 미검증 |
| SET-3-03 Git 이메일(-184) | – | CLI 등록·조회 | Agent 대조용 SHA-256 확인 | – (CLI 단계) |
| IDX-3-01~03 커밋·작업 파일 수집과 범위 제한(-185~-187) | – | 미구현 | 미검증 | 미검증 |
| FIND-3-01 find\_agent 중계 | 통과 | 통과 | 통과 | 통과(도구 호출, 두 Host) |
| FIND-3-02 결과 표시 | 통과(스키마) | 통과 | 통과 | 도구 결과 통과 / 표시 문구는 수동 확인 |
| REQ-3-01 후보 선택 → create\_task → 수락·완료 | 통과 | 통과 | 통과 | 통과(도구 호출: Codex 요청, Claude 수락·완료) |

## 다섯 경우의 후보·점수

역할 태그는 A(Codex)에 `FRONTEND`/`["화면"]`, B(Claude Code)에 `BACKEND`/`["로그인", "결제 API"]`를 줬다. 아래는 SDK 단계에서 단언하는 실제 중앙 응답이다.

| 경우 | 질문 | 결과 |
| --- | --- | --- |
| 역할 태그만 | A: "로그인 오류를 고쳐줄 담당자" | MATCHED, B 1명. part `BACKEND`, ONLINE, 총점 20 = 역할 20(태그 `로그인`) + Git 0 |
| 역할 태그만(반대 방향) | B: "결제 화면 레이아웃" | MATCHED, A 1명. 태그 `화면`, 총점 20. 요청자 B는 후보에서 빠진다 |
| 근거 부족 | A: "배포 파이프라인 캐시 설정" | INSUFFICIENT\_EVIDENCE / NO\_MATCHING\_EVIDENCE. 참고 후보는 B 1명, 0점 |
| Git 미수집 | 위 세 경우 모두(각각 단언) | `gitCoverage` = NOT\_COLLECTED / NO\_TEAM\_REPOSITORIES, workingState NOT\_COLLECTED |
| Git 근거만 | – | **미검증.** 수집(-185~-187)이 없다 |
| 역할 + Git | – | **미검증.** 같은 이유 |

업로드 페이로드에 절대 경로·diff·이메일 원문이 없는지 검사하는 항목도 **미검증**이다. 아직 업로드가 없다.

## 실제 Host 결과

1. **Codex 탐색**: `register_agent` → `find_agent`만 호출했다.
   - 수동 확인: 후보 표에 이름·agentId·조회 시 연결 상태·총점·역할/Git 점수·일치 역할을 보여줬다. Git 미수집과 작업 파일 미관측을 따로 설명했고, "이 후보를 선택하시겠어요?"라고 묻고 작업을 보내지 않았다.
2. **Claude Code 탐색**: 같은 두 도구만 호출했다.
   - 수동 확인: 점수 내역, 접속 상태가 점수에 들어가지 않는다는 점, `NO_TEAM_REPOSITORIES`를 설명했다.
3. **Codex 전송**: 사용자가 B를 골랐다는 지시를 받고 `register_agent` → `create_task`만 호출했다. 수신자는 B였다.
   - 그때 B에는 바인딩된 MCP 세션이 없어 `DELIVERY_NOT_SENT`/`RECEIVER_OFFLINE`이 왔다. A를 다시 바인딩해 확인한 시점(약 80초 뒤)에도 ACK는 없었다. spec이 두 값을 모두 단언한다.
   - Codex는 이를 그대로 보고하고 재시도하지 않았다. [-190 지침](agent-workflow.md#담당자를-모를-때-find_agent-후보에서-사용자가-고른다-s15p31d108-190)과 일치한다.
4. **Claude Code 수락·완료**: `register_agent` → `get_task_result` → `update_task_status(IN_PROGRESS)` → `complete_task`.
   - 작업 생성 뒤 B를 처음 바인딩한 것이 이 Claude 턴이었다.
   - 그 뒤 요청자 쪽에서 `ACK_RECEIVED`와 COMPLETED를 확인했다.

참고: 실제 Host 단계에서는 탐색 결과의 상대 Agent가 OFFLINE으로 나왔다(annotation `candidateConnectionStatus`). 각 Host 턴 사이에는 해당 Agent에 바인딩된 MCP 세션이 없기 때문이다. Host를 띄워 두지 않은 팀원은 추천에서 OFFLINE으로 보인다.

## 발견한 문제

- **[#108](https://github.com/SSAFY-URAYAHA/DevLink_client/issues/108)**: 같은 Agent의 Host 턴이 끝난 뒤 다음 바인딩까지 걸린 시간이 Host마다 크게 달랐다. 아래 값은 세션 해제 수정 전 두 번 실행한 기록이다.

  | 직전 Host 턴 | 다음 바인딩까지 | 실행 중 MCP 프로세스 | 종료 시 MCP 프로세스 |
  | --- | --- | --- | --- |
  | Codex `exec` | 78·86초, 83·83초 | 1 | 0 |
  | Claude Code `-p` | 3·3초, 4·3초 | 1 | 0 |

  - 검출기는 실행 중인 MCP 서버를 매번 잡았다(spec이 단언한다). 그래서 종료 시 0개는 프로세스가 실제로 끝났다는 뜻이다.
  - Codex는 MCP 프로세스가 끝났는데도 세션이 풀리지 않았다. MCP 세션을 해제하지 않고 끝내서, 90초 lease가 끝날 때까지 `SESSION_CONFLICT`가 나는 것으로 추정한다.
  - 대기 없이 연속 실행한 첫 시도는 두 도구 호출이 모두 거부됐다.
  - 세션 해제 수정 [PR #111](https://github.com/SSAFY-URAYAHA/DevLink_client/pull/111)을 포함한 통합 후보를 격리 중앙에서 다시 실행했다. Codex 두 턴 뒤에는 각각 5초, Claude Code 두 턴 뒤에는 각각 4초 만에 다음 세션이 바인딩됐다.
- MCP 클라이언트는 `SESSION_CONFLICT`를 종료 상태로 취급한다. 그래서 E2E는 바인딩을 시도할 때마다 새 MCP 세션을 연다.

## 남은 일

- 선행 스토리가 병합되면 같은 시나리오에 다음을 추가한다.
  - IDX-3-01~03 수집과 등록한 Git 신원의 작성자 대조
  - Git 근거만 / 역할+Git 두 경우
  - 업로드 페이로드 검사
- 2026-11-12 갱신(S15P31D108-242): E2E 고정 커밋을 `d945f8b`에서 server develop `65db99a`(작업 파일 revision 계약, DevLink_server#96 포함)로 올리고, Git 근거만·역할+Git 두 경우를 [E2E S18](../../../e2e/specs/p1-s18-git-evidence.spec.ts)로 확인했다. 등록한 Git 이메일로 만든 커밋이 본인 커밋으로 집계되는 것도 S18의 빈도·최근성 점수로 확인했다. 업로드 페이로드 검사는 E2E에 없고 브로커 단위 테스트가 다룬다.
- 고정 중앙 `d945f8b`의 S15에서 연결된 후보의 `ONLINE`과 브로커 중지 뒤의 `OFFLINE`을 확인했다.
- 이 기록만으로 -191 수용 기준을 완료로 보지 않는다.
