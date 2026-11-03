# Codex·Claude Code Host 호환성 (S15P31D108-64)

## 검증 범위

2026-10-28, Windows의 실제 Codex CLI와 Claude Code가 동일한 빌드의 MCP 서버를 stdio 자식 프로세스로 실행한다. `scripts/verify-hosts.mjs`는 두 Host를 하나의 로컬 HTTP 테스트 브로커에 연결한다. 서버·Host를 SDK Client로 대체하지 않으며, 중앙 서비스의 실제 데이터를 사용하지 않는다.

| 계층 | 증거와 한계 |
| --- | --- |
| SDK → MCP → HTTP 테스트 브로커 | 기본 `npm test`의 자동 회귀, 프로토콜·동시성·복구 경쟁 검사 |
| 실제 Host → MCP → 동일 HTTP 테스트 브로커 | 별도 `test:hosts` 실행, 실제 도구 이벤트·HTTP 요청·종료 프로세스 대조 |
| 실제 Host → MCP → Rust 브로커 | 현재 Rust가 `/health`·`/shutdown`만 구현하여 `/v1` 세션·업무 API와 신원 전달 미검증 |
| 실제 중앙 서비스 업무 왕복 | 미검증. 테스트 브로커의 결과를 실제 연동 완료로 간주하지 않음 |

Jira 완료 판정에는 실제 Rust에 등록 Agent 식별자가 전달되는 확인이 추가로 필요하다. 이번 스토리는 Host 검증 실행기·재현 설정·관찰 결과를 제공하며, -50 담당자의 브로커·자격 파일 발행이나 -65/-66의 업무 수신·수락·로컬 실행을 구현하지 않는다.

## 실행 환경과 설정

| 항목 | 실제 확인 값 |
| --- | --- |
| OS / 전송 | Windows / stdio |
| Node.js | 24.18.0 |
| Codex CLI | 0.165.0-alpha.1.2 |
| Claude Code | 2.1.310 |
| Claude 모델 / effort | 실제 init 모델 `claude-opus-5-5` / CLI 요청 effort `high` (init에 effort 미표시) |
| Codex 모델 | CLI 기본값, JSONL에 모델 ID가 없어 특정 모델 검증으로 표기하지 않음 |

Host 설정 예제는 [패키지 README](../README.md#mcp-host-설정-예시)를 따른다. 각 Host에 별도 CLI 발행 프로필 파일을 지정하며, 토큰 값은 명령·프롬프트에 넣지 않는다. 임시 검증에서는 private DACL 디렉터리 안에 합성 자격 파일을 만들고 `DEVLINK_BROKER_FILE` 경로만 전달한다. 실제 브로커의 `controlToken`을 MCP 자격으로 전환하지 않는다.

일회성 설정은 `test/helpers/host-transcript.mjs`의 `hostConfiguration`을 사용한다.

- Codex: `--no-daemon`, `exec --ignore-user-config --ephemeral --sandbox read-only --json`, 단일 `mcp_servers.devlink` 인라인 설정과 명시적인 8개 도구 목록. 이 테스트의 MCP 도구만 승인하며 사용자 설정을 수정하지 않는다. 공유 데몬 경로는 이번 실행 범위에 포함하지 않는다.
- Claude: `--restricted --strict-mcp-config --mcp-config <임시 JSON>`, 기본 도구 비활성화(`--tools ""`), DevLink 8개 도구만 허용, `stream-json --verbose`, 세션 저장 비활성화. 실제 모델 ID는 init 이벤트로 확인한다.
- 서버: 프로덕션 `dist/index.js --transport stdio`, 검증용 HTTP 제한 200ms·전체 호출 제한 3000ms. 이 값은 빠른 장애 주입용이며 운영 SLA가 아니다.

공식 설정 출처: [Codex MCP](https://learn.chatgpt.com/docs/extend/mcp), [Claude MCP](https://code.claude.com/docs/en/mcp), [Claude CLI](https://code.claude.com/docs/en/cli-reference). CLI 플래그는 위 버전에서 확인했으며 다른 버전은 재실행으로 확인한다.

## 재현

저장소 루트의 PowerShell에서 실행한다. 네이티브 `node.exe`, `codex.exe`, `claude.exe`가 PATH에 있고 각 CLI 로그인이 완료되어 있어야 한다. `.cmd` shim 설치는 이 실행기의 지원 대상이 아니다. 실제 모델 호출은 비용·사용량을 소모하므로 기본 테스트에 포함하지 않는다.

```powershell
npm --prefix packages/mcp-server ci
npm --prefix packages/mcp-server run check
npm --prefix packages/mcp-server test

# 선택 실행: root에서 직접 호출하여 산출물 위치를 고정한다.
node packages/mcp-server/scripts/verify-hosts.mjs --host both `
  --output .planning/mcp-64/host-report.json `
  --artifacts .planning/mcp-64/host-raw
```

`.planning`은 로컬 검증 산출물용이며 원본 JSONL은 커밋·공유하지 않는다. JSONL에는 CLI 계정 메타데이터가 포함될 수 있다. `--artifacts`를 생략하면 원본을 저장하지 않는다. `--host codex` 또는 `--host claude`로 한 Host만 검사할 수 있다. 각 CLI 실행 제한은 기본 180초, `--timeout-ms`는 10000–300000ms다. 실패 시 종료 코드 1과 실패 Host/단계를 보고한다. 중단·시간 초과·출력 초과 시 살아 있는 소유 Host의 프로세스 트리를 정리하고, stdio가 닫히지 않아도 제한된 대기 후 MCP marker 정리로 진행한다.

`npm --prefix packages/mcp-server run test:hosts -- --host both`도 가능하다. 이 경우 실행 디렉터리가 패키지이므로 `--output`·`--artifacts`는 절대 경로를 사용한다. 사용자 전역 Host 설정이나 프로젝트 설정 파일을 새로 등록하지 않는다.

## 공통 시나리오와 판정

각 Host는 ping → 16단계 업무·오류 → 토큰 교체 후 새 프로세스 → 브로커 미기동의 네 번의 독립 실행을 수행한다. 모델의 완료 선언만으로 통과하지 않는다. 실제 도구 호출 수·순서·입력, 모델이 받은 결과, HTTP의 신뢰된 컨텍스트·상관 ID를 대조한다.

| 시나리오 | 검사 내용 |
| --- | --- |
| 초기화·목록·ping | 당시 Claude init의 8개 도구 목록(현재 verifier 대조는 10개), 두 Host에서 ping+7개 업무 도구 실제 호출, ping의 브로커 요청 0건 |
| 등록·목록·생성·조회 | Host별 `userId/teamId/agentId/toolType`을 HTTP 테스트 브로커에서 검증, 생성 직후 PENDING과 전달 UNKNOWN 구분 |
| FAILED 입력 오류 | reason 생략을 의도적으로 호출, SDK 입력 오류가 Host에 노출되고 브로커에 도달하지 않음 |
| 명시적 상태 전이·완료 | IN_PROGRESS 및 complete_task만 전달, 장애 때문에 FAILED를 자동 전송하거나 create_task를 재전송하지 않음 |
| 업무 권한 거부 | BROKER_FORBIDDEN/rejected, 다음 정상 조회 성공 |
| HTTP 지연 | BROKER_TIMEOUT/unknown, 다음 정상 조회 성공, 중앙 상태를 실패로 추정하지 않음 |
| 브로커 상태 소실 | 연결 종료 뒤 다음 호출에서 recover, 동일 MCP instanceId 유지 |
| 자격 거부 | BROKER_UNAUTHORIZED/rejected, 같은 프로세스의 다음 호출도 동일 인증 오류 표시 |
| 자격 교체·재시작 | 기존 토큰 폐기, 새 private 파일을 새 MCP 프로세스가 읽음, 종료 시 DELETE 관찰 여부를 별도 기록. -142 이후 fixture는 회전 시 브로커를 재시작해 이전 소유권을 비움 |
| 브로커 미기동 | 닫힌 loopback 포트의 조회가 BROKER_UNAVAILABLE/unknown으로 노출 |
| 종료 | 시작 시 marker 조회의 양성 대조군 검증, 고유 Node marker로 각 실행의 잔여 MCP 자식 0개 확인, 강제 정리가 필요하면 실패 |

테스트 브로커는 오류를 주입하기 전에 토큰·세션·principal·Agent 바인딩을 검증한다. 이는 MCP의 전달 동작을 검사하는 합성 구현이며 Rust가 동일한 검증을 제공한다는 증거는 아니다. 두 Host는 같은 브로커에 서로 다른 프로필로 순차 연결한다. 실제 Host의 동시 소유권 경쟁은 이 시나리오에 포함하지 않는다.

상관관계 검사는 비어 있지 않고 중복되지 않는 HTTP requestId와 MCP 요청 ID의 HTTP→도구 결과 echo 일치를 확인한다. CLI의 tool-use/item ID와 MCP wire JSON-RPC ID는 별개이며, CLI가 원본 wire ID를 노출하지 않아 원본 ID 일치 검증으로 표기하지 않는다. 브로커 미기동 검사는 닫힌 임시 포트를 사용하므로 다른 프로세스의 포트 선점 가능성이 남으며, 이 단계 전에 테스트 토큰을 폐기한다.

## 실제 Host 이벤트의 차이

첫 실행에서 다음 차이를 발견해 검증기와 회귀 테스트에 반영했다. 서버 wire 응답은 변경하지 않았다.

- Codex의 SDK 입력 오류: 완료 이벤트의 `status: "failed"`, `structured_content: null`, `error: null`과 plain text 본문으로 보고된다. `isError`만 검사하면 입력 오류를 놓친다.
- Claude의 업무 오류: 모델이 받은 `message.content[].tool_result.content`에는 JSON 오류가 그대로 있지만, 편의 필드 `tool_use_result.content`는 `Error: `를 앞에 붙이고 structuredContent를 생략한다. 상관된 tool_result 본문을 판독하여 BROKER_* 코드와 안전한 메타데이터를 보존한다.
- 성공 결과의 JSON text와 structured 결과가 둘 다 있으면 일치해야 한다. 다른 서버·내장 파일/명령 도구 사용, 추가 재시도, 미완료 호출, 원본 호출 없는 결과는 통과하지 않는다.

POSIX, GUI/IDE Host, 다른 CLI 버전, 실제 Rust 재시작·중앙 ACK·동시 Host 소유권은 별도 검증이 필요하다. 자격 거부 이후에는 CLI가 자격 파일을 갱신하고 MCP 프로세스를 명시적으로 재시작한다.

Windows 최종 실행에서 성공한 조회 뒤 Codex의 세션 DELETE는 미관찰, Claude의 DELETE는 관찰되었고 잔여 MCP 프로세스는 양쪽 모두 0개였다. 앞선 Claude 실행에서는 DELETE가 미관찰되어 종료 시 항상 전달된다고 보장하지 않는다. [Claude 공식 종료 정책](https://code.claude.com/docs/en/mcp#how-claude-code-stops-a-stdio-server)은 stdin을 닫고 300ms 뒤 프로세스 트리를 종료한다고 설명한다. 이 실행에서 어떤 종료 경로가 먼저 발생했는지는 단정하지 않는다. MCP의 best-effort 해제만으로 소유권 정리를 보장할 수 없으므로 브로커가 heartbeat TTL 만료·fresh/recover 정책을 제공해야 한다. SDK의 정상 EOF 해제 검증과 실제 Host의 종료 제약은 별도 결과로 기록한다.

## 2026-10-28 실행 결과

TypeScript `check`와 기본 회귀 88개가 통과했다. 지정 버전의 두 실제 Host를 동일 테스트 브로커로 실행한 최종 보고서는 `passed: true`였다. 이 보고서는 마지막 중단·프로세스 조회 실패 경로 보강 직전 실행기의 결과이며, 이후 성공 경로는 바뀌지 않았다. 마지막 보강은 captureHost abort 회귀와 정적 교차검토로 확인했고 실행기 전체의 중단·조회 실패 경로는 정적 검토 범위다.

| 단계 | Codex | Claude Code |
| --- | --- | --- |
| 초기화·목록·ping, broker 도착 0건 | 통과 | 통과 |
| 업무·입력 오류·권한·timeout·복구·인증 거부 16단계 | 통과 | 통과 |
| HTTP 신원 컨텍스트·상관 ID echo | 통과 | 통과 |
| 자격 교체 후 새 프로세스 | 통과 | 통과 |
| 브로커 미기동 오류 | 통과 | 통과 |
| 각 실행 후 잔여 MCP 프로세스 | 0개 | 0개 |
| 종료 시 세션 DELETE 도착 | 미관찰: lease 만료 필요 | 최종 실행에서 관찰, 이전 실행에서는 미관찰 |

첫 시도들의 오류 포장 판독 실패와 Codex 모델 용량 오류는 통과로 처리하지 않았으며, 수정·재실행 후 결과를 기록했다. CLI의 부가 설명 뒤 최종 JSON 코드 블록이 오는 출력도 처리하지만, 실제 도구 이벤트가 없거나 중첩되면 통과하지 않는다. 보고서는 CLI 원본 wire ID를 관찰하지 않았음을 `hostWireRpcIdObserved: false`로 구분한다.
