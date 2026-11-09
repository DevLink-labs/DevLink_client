# MCP Prompts Host 지원 조사 (S15P31D108-145)

## 목적과 결론

선택형 설정 Prompt(`devlink_setup`)를 도입하기 전에 Codex·Claude Code가 MCP Prompts를 실제로 발견·표시·호출하는지 확인했다. 그 결과를 근거로 Claude Code 한정 채택을 결정하고 구현했다. 이 문서는 수용 기준 ①의 근거, 구현 계약, 실제 Host 검증 기록이다.

| Host (버전) | `prompts/list` | 사용자 노출 | `prompts/get` 호출 | 판정 |
| --- | --- | --- | --- | --- |
| Claude Code 2.1.310 → 2.1.319 | 시작 시 호출 | `/mcp__<서버>__<prompt>` 슬래시 명령 | 사용자가 명령을 입력했을 때만 호출 | 지원 |
| Codex CLI 0.166.0 → 0.167.0 | `exec`·대화형 TUI 모두 미호출 | 없음 | 미호출 | 미지원 |

- Claude Code는 prompt를 슬래시 명령으로 노출하고, 사용자가 명령을 입력할 때만 실행한다. 모델이 prompt를 스스로 호출하는 경로는 관찰되지 않았다.
- Codex는 서버가 `prompts` capability를 광고해도 prompt 목록을 요청하지 않는다. Codex 사용자는 Prompt 없이 `get_connection_status` → `get_my_context` → `register_agent({})` 도구 흐름을 쓴다(수용 기준 ④). 이 흐름은 실제 Codex로 다시 확인했다.
- 결정(2026-11-09, 임효민): **Claude Code 한정 채택**. 상세는 [채택 판단](#채택-판단), 구현은 [devlink_setup 구현](#devlink_setup-구현)에 있다.

## 실행 환경

| 항목 | 값 |
| --- | --- |
| 조사일 | 2026-11-09 (Asia/Seoul) |
| OS / 전송 | Windows 11 / stdio |
| Node.js | 24.18.0 (`C:/Program Files/nodejs/node.exe`) |
| Claude Code | 조사 2.1.310, 구현 검증 2.1.319 (조사 중 자동 업데이트) |
| Codex CLI | 조사 0.166.0, 구현 검증·미지원 재확인 0.167.0 (조사 중 업데이트) |
| MCP SDK | `@modelcontextprotocol/server` 2.4.0 (패키지 고정 버전) |
| Claude 모델 / effort | init 이벤트의 `claude-opus-5-5` / `low` (조사용 호출이라 effort를 낮춤) |
| Codex 모델 | CLI 기본값. `exec --json`에 모델 ID가 없어 특정 모델로 표기하지 않음 |

## 조사 방법

### 임시 probe 서버

DevLink 서버를 수정하지 않고 저장소 밖 임시 디렉터리에 probe 서버를 만들었다. 패키지와 같은 SDK(`McpServer`, `StdioServerTransport`)로 다음을 등록하고, Host가 보내는 모든 JSON-RPC 메서드를 `PROBE_LOG` 파일에 한 줄씩 기록했다.

- 도구 `probe_echo`: 고정 문자열 `TOOL-MARKER-7731` 반환
- Prompt `probe_setup`: 선택 인자 `focus`. `PROMPT_RECEIVED 4419 focus=<값>` 한 줄로 답하라는 user 메시지 반환
- Prompt `probe_plain`: 인자 없음. `PLAIN_RECEIVED 5520`으로 답하라는 user 메시지 반환

SDK Client 대조 실행에서 서버 capability는 `{"prompts":{"listChanged":true},"tools":{"listChanged":true}}`였고, `prompts/list`·`prompts/get`이 정상 응답했다. 이후 차이는 Host 동작으로 본다.

### 사용자 설정 격리

- Claude Code: `--restricted --strict-mcp-config --mcp-config <임시 JSON>`, `--tools ''`, `--allowedTools mcp__probe__probe_echo`, `--permission-mode dontAsk`, `--no-session-persistence`(비대화형만). 사용자 `~/.claude.json`의 MCP 설정을 읽거나 바꾸지 않는다.
- Codex `exec`: `--no-daemon -a never exec --ignore-user-config --ephemeral --skip-git-repo-check --sandbox read-only --json`과 `-c mcp_servers={probe={...}}` 인라인 설정. `scripts/verify-hosts.mjs`의 `hostConfiguration`과 같은 방식이다.
- Codex 대화형 TUI: `--ignore-user-config`가 없어 사용자 설정은 읽지만 수정하지 않는다. `-c mcp_servers.probe={...}`와 `-c check_for_update_on_startup=false`만 덧붙였다.
- Git Bash에서 실행할 때는 `MSYS_NO_PATHCONV=1`이 필요하다. 없으면 `/mcp__...` 인자가 `C:/Program Files/Git/mcp__...`로 바뀐다. 첫 Claude 실행이 이 때문에 무효였고, 결과에서 제외했다.

## 관찰 결과

### Claude Code 2.1.310

비대화형 `-p`, `--output-format stream-json --verbose`.

```bash
MSYS_NO_PATHCONV=1 claude -p --model claude-opus-5-5 --effort low --restricted \
  --strict-mcp-config --mcp-config <임시>/mcp.json --tools '' \
  --allowedTools mcp__probe__probe_echo --permission-mode dontAsk \
  --no-session-persistence --no-chrome --output-format stream-json --verbose \
  "/mcp__probe__probe_setup alpha" < /dev/null
```

| 실행 | probe 서버 수신 순서 | 결과 |
| --- | --- | --- |
| `/mcp__probe__probe_setup alpha` | `server/discover` → `initialize` → `notifications/initialized` → `tools/list` → `prompts/list` → `prompts/get {name:"probe_setup", arguments:{focus:"alpha"}}` | 최종 답 `PROMPT_RECEIVED 4419 focus=alpha` |
| `/mcp__probe__probe_plain` | 위와 같고 `prompts/get {name:"probe_plain"}` | 최종 답 `PLAIN_RECEIVED 5520` |
| 경로가 변형된 일반 문장(무효 실행) | `prompts/list`까지만, `prompts/get` 없음 | 모델이 "`probe_setup` 도구가 없다"고 답함 |

- 발견: `system/init` 이벤트의 `slash_commands`에 `mcp__probe__probe_setup`, `mcp__probe__probe_plain`이 들어 있다. `tools`에는 `mcp__probe__probe_echo`만 있다. Prompt는 모델 도구 목록에 들어가지 않는다.
- 인자: 슬래시 명령 뒤의 위치 인자 `alpha`가 선언 순서대로 `focus`에 매핑되었다.
- 결과 전달: `prompts/get`의 user 메시지가 대화 입력으로 들어가고, 모델은 그 지시대로 답했다.
- 암묵 호출: 사용자가 슬래시 명령을 입력하지 않은 실행에서는 `prompts/get`이 한 번도 오지 않았다. `prompts/list`는 세션 시작마다 호출된다.
- 시작 시점: `initialize`에서 Claude Code는 `roots`, `elicitation` capability를 광고하고 protocol `2025-11-25`를 쓴다.

대화형 TUI(`claude ... "/mcp__probe__probe_setup tui"`를 Terminal 탭에서 실행):

- probe 서버는 `prompts/list`까지 받았지만 화면에는 `Unknown command: /mcp__probe__probe_setup`이 표시되었다. 시작 인자로 넘긴 슬래시 명령이 MCP prompt 목록을 받기 전에 처리된 것으로 보인다(추정).
- 따라서 대화형에서는 세션이 열린 뒤 사용자가 직접 입력해야 한다. 문서에도 시작 인자 대신 입력 방식을 안내해야 한다.
- 대화형에서 `/` 입력 후 자동완성 목록에 prompt가 보이는지는 키 입력 자동화 도구(PTY)가 없어 직접 확인하지 못했다. 미검증 항목에 남긴다.
- 대화형은 `--no-session-persistence`를 쓸 수 없어(`--print` 전용) 해당 실행의 세션 기록이 사용자 Claude 기록에 남았다.

### Codex CLI 0.166.0

`exec`:

```bash
MSYS_NO_PATHCONV=1 codex --no-daemon -a never exec --ignore-user-config --ephemeral \
  --skip-git-repo-check --sandbox read-only --json --cd <임시> \
  -c 'mcp_servers={probe={command="<node>",args=["<임시>/probe-server.mjs"],env={PROBE_LOG="<임시>/a.log"},enabled_tools=["probe_echo"],required=true,default_tools_approval_mode="approve",startup_timeout_sec=20,tool_timeout_sec=10}}' \
  -c mcp_optional_startup_grace_ms=0 "/mcp__probe__probe_setup alpha" < /dev/null
```

| 입력 | probe 서버 수신 | 결과 |
| --- | --- | --- |
| `/mcp__probe__probe_setup alpha` | `initialize` → `notifications/initialized` → `tools/list` | 모델이 `probe_setup`을 쓸 수 없다고 답함 |
| `/prompts:probe_setup alpha` (Codex 사용자 정의 prompt 형식) | 위와 같음 | 모델이 셸로 `~/.codex/prompts`를 찾으려다 정책에 막힘. prompt를 찾지 못함 |
| "probe_echo를 한 번 호출하고 보이는 MCP prompt 이름을 나열하라" | 위 + `tools/call` | `probe_echo`는 호출됐고 prompt 목록 답은 `NONE` |

- 세 실행 모두 `prompts/list`와 `prompts/get`이 오지 않았다.
- `initialize` 요청의 clientInfo는 `codex-mcp-client` 0.166.0, protocol `2025-06-18`, capability는 `elicitation`과 `experimental.codex/auth-change`뿐이다.
- `/prompts:<이름>`은 `~/.codex/prompts`의 로컬 파일을 쓰는 Codex 자체 기능이며 MCP Prompts와 관계없다.

대화형 TUI(Terminal 탭, 시작 후 약 20초 대기):

- probe 서버 수신은 `initialize` → `notifications/initialized` → `tools/list`뿐이다. `prompts/list`는 없었다.

앱 서버 프로토콜(`codex app-server generate-ts`로 생성한 바인딩):

- 클라이언트 요청 중 MCP 관련은 `mcpServerStatus/list`, `mcpServer/tool/call`, `mcpServer/resource/read`, `mcpServer/oauth/login`뿐이다.
- `McpServerStatus` 타입은 `tools`, `resources`, `resourceTemplates`만 갖고 prompt 필드가 없다.
- TUI·데스크톱 앱 같은 앱 서버 클라이언트도 이 버전에서는 MCP prompt를 받을 경로가 없다고 판단한다(바인딩 기준 추정, 데스크톱 앱 실행은 미확인).

0.167.0 재확인: 같은 `exec` 명령(`/mcp__probe__probe_setup alpha`)에서 probe 서버 수신은 `initialize` → `notifications/initialized` → `tools/list`뿐이었다. 0.167.0으로 다시 생성한 앱 서버 바인딩도 MCP 요청 4종이 같고, `McpServerStatus`에 prompt 필드가 없다.

## 설계에 미치는 영향

- Claude Code는 세션마다 `prompts/list`를 호출한다. 서버가 `prompts` capability를 켜면 이 요청이 브로커를 깨우지 않아야 한다. 현재 `verify-hosts.mjs`의 "Catalog/ping activated the broker" 단언과 같은 조건이다.
- Prompt 결과는 user 메시지로 들어가 모델이 이어서 행동한다. Prompt 자체는 도구를 실행하지 않지만, 모델이 안내문을 읽고 `register_agent` 등을 바로 호출할 수 있다. 수용 기준 ③을 지키려면 안내문에 "사용자 확인 전 생성·회전·업무 실행 금지"를 명시하고, 실제 Host 회귀에서 `prompts/get` 뒤 쓰기 도구 호출이 없는지 검증해야 한다.
- 현재 진단에 따라 단계를 고르는 방법(수용 기준 ②)은 두 가지다. `prompts/get` 처리 중 서버가 `getConnectionStatus`·`getMyContext`를 읽기 전용으로 호출해 필요한 단계만 넣는 방식과, 고정 안내문이 모델에게 진단 도구를 먼저 부르게 하는 방식이다. 전자가 결과가 결정적이고 회귀 검증이 쉽다.
- 대화형 Claude Code에서 시작 인자로 넘긴 prompt 명령은 실패할 수 있다. 사용 안내는 "세션을 연 뒤 `/mcp__devlink__devlink_setup` 입력"으로 적는다.
- Codex에서는 Prompt를 쓸 수 없으므로 기존 도구 흐름 문서가 기본 경로로 남는다. Prompt는 Claude Code 사용자를 위한 선택 진입점이다.

## 채택 판단

| 선택지 | 내용 | 장점 | 비용·위험 |
| --- | --- | --- | --- |
| A. Claude Code 한정 채택 (권장) | `devlink_setup` 구현. Codex는 미지원으로 문서화하고 도구 흐름 유지 | 실제 지원이 확인된 Host에서 온보딩 단계를 줄임. 명시적 슬래시 명령이라 암묵 실행 위험이 낮음 | `prompts` capability·회귀 테스트·실제 Claude Host 회귀 추가. Codex 사용자 경험은 그대로 |
| B. 미채택 | 이 조사를 결정 기록으로 남기고 도구 흐름만 유지 | 구현·유지 비용 없음 | 수용 기준 ②⑤의 Prompt 관련 항목은 해당 없음으로 종료 |
| C. 보류 | Codex가 MCP Prompts를 지원할 때 재조사 | 두 Host 동일 경험 | 일정 미정. 재조사 필요 |

결정: 2026-11-09(Asia/Seoul), 결정자 임효민, **A. Claude Code 한정 채택**. Codex는 미지원으로 기록하고 기존 도구 흐름을 기본 경로로 유지한다. Codex가 MCP Prompts를 지원하면 같은 probe 절차로 다시 확인한다.

## devlink_setup 구현

### 계약

| 항목 | 값 |
| --- | --- |
| 이름 / 제목 | `devlink_setup` / `DevLink setup` |
| 인자 | 없음 (`arguments` 미노출). 사용자 입력·비밀값을 받지 않는다 |
| Claude Code 호출 | 세션이 열린 뒤 `/mcp__devlink__devlink_setup` 입력 |
| 서버 capability | `prompts` 추가. `tools`는 그대로 |
| `prompts/list` | 브로커·자격 파일에 접근하지 않는다 |
| `prompts/get` | `get_connection_status`와 같은 읽기 전용 진단을 한 번 수행한다(`GET /v1/connection`). 세션 claim·bind·lease 갱신·업무 요청 없음 |
| 결과 | `role: user` 텍스트 메시지 1개: 진단 요약, 필요한 단계 번호 목록, 안전 규칙 |
| 코드 | `src/prompts/setup.ts`, 등록은 `src/server.ts` |

`get_my_context`는 prompt 처리 중 호출하지 않는다. 이 호출은 MCP 프로세스에 관측 문맥을 고정하므로, 사용자가 등록 단계를 고른 뒤 모델이 도구로 호출해 Agent 이름·도구·프로젝트를 보여 주게 했다(수용 기준 ②의 "문맥" 단계).

### 단계 선택 규칙

- 준비 완료: `hostSession=bound`, `identity=verified`, `credentials=valid`, `broker=running`, `central=Connected`이고 `issue`·`clientIssue`가 없으면 단계를 보여 주지 않는다.
- 그 밖에는 진단의 `nextActions`를 순서대로, 중복 없이 단계로 보여 준다.
- `review_mcp_config`는 뺀다. Rust 진단은 Host 설정을 볼 수 없어 이 항목을 항상 붙이는데, prompt는 Host가 읽은 DevLink MCP 설정을 통해 도착했으므로 설정이 로드된 것은 이미 확인된 셈이다. 자격 파일 경로가 틀린 경우는 `credentials`·`identity` 단계로 드러난다.
- 남는 단계가 없는데 준비 완료도 아니면 `retry_diagnosis`를 보여 준다.
- `clientIssue`가 있으면(이 MCP 프로세스의 영구 차단) `bound`여도 준비 완료로 보지 않는다.

`nextActions` 14종 각각에 고정 안내문이 있다(`setupGuidance`). `register_agent`만 "사용자가 요청할 때 assistant가 실행"이고 나머지는 모두 "사용자가 직접 실행"이다.

아래 CLI 하위 명령은 모두 `devlink --config "<PROFILE_CONFIG>"` 뒤에 붙인다. `<PROFILE_CONFIG>`에는 이 Host의 `DEVLINK_BROKER_FILE`을 발행한 CLI 설정 파일의 절대 경로를 넣는다. `devlink --config "<PROFILE_CONFIG>" mcp path` 출력과 Host 설정의 경로를 대조할 수 있다. 경로가 불명확하거나 처음 설정할 때는 사용자가 대상 프로필을 확인한 뒤 진행한다. 기본 프로필도 사용자가 대상으로 확인한 경우에만 지정한다.

| nextAction | 사용자에게 보여 주는 안내 요지 |
| --- | --- |
| `prepare_cli_profile` | 터미널에서 `setup` 실행 후 Host 재시작 |
| `repair_private_file` | `setup status`로 자격 파일 확인·복구. 파일 내용은 비공개로 유지 |
| `inspect_local_broker` | `status`로 확인하고 멈췄으면 `start` |
| `check_network_then_retry` | 네트워크·중앙 상태 확인 후 prompt 재실행 |
| `stop_then_reauthenticate_agent` | 사용자가 직접 `stop` → `agent token` → `start` → Host 재시작 |
| `inspect_profile_then_rebind` | `setup status`로 확인. 의도한 Agent·프로젝트면 Host만 재시작. 아니면 `setup` 재실행, 또는 `stop` → `agent use --id <ID> --project <PATH>` → `agent token` → `start` 후 Host 재시작(`agent use`는 멈춘 브로커가 필요하고 Agent 토큰을 다시 요구함) |
| `inspect_connection_owner` | `central=Replaced`이면 같은 Agent를 사용하는 브로커가 있는지 확인하고 소유자와 조율한다. 충돌 해소 후 `stop` → `start`로 대상 브로커를 재시작하고 Host를 다시 연다. 로컬 MCP 세션 충돌이면 소유 프로세스가 멈춘 뒤 약 90초 lease 만료를 기다린다. |
| `check_provider_contract` | 브로커와 MCP 서버를 같은 릴리스로 맞춤 |
| `repair_inbox_then_restart` | `status` 확인, 수리 후 `stop` → `start` |
| `register_agent` | 요청 시 `get_my_context({})`로 Agent 확인 → 사용자 확인 뒤 `register_agent({})` |
| `review_mcp_config` | `setup status`로 DevLink MCP 항목 확인 (위 규칙상 보통 표시되지 않음) |
| `open_or_restart_host` | 앞 단계 뒤 Host 재시작 |
| `inspect_owned_agents_then_use` | `agent list` 확인 후 `setup --agent-id <ID>`로 기존 Agent 선택 |
| `retry_diagnosis` | prompt 재실행, 반복되면 `status` |

안내에 쓴 CLI 명령(`devlink setup`, `setup status`, `setup --agent-id`, `status`, `start`, `stop`, `agent list`, `agent use --id --project`, `agent token`)은 `rust/crates/devlink-cli`의 명령 정의에서 실제로 있는지 확인했다.

### 안전 규칙 (수용 기준 ③)

prompt 결과는 user 메시지로 모델에 들어가므로, 메시지 끝에 다음 규칙을 고정으로 붙인다.

- 이번 응답에서는 진단과 단계만 보여 주고 사용자에게 어느 단계를 할지 묻는다. 도구를 호출하지 않는다.
- `get_my_context({})`·`register_agent({})`는 사용자가 그 단계를 명시적으로 요청한 뒤에만 호출한다. `register_agent`는 `{}`만 쓴다.
- 비밀번호·토큰·자격 파일 내용을 요구·수용·반복하지 않는다.
- Agent 생성, 토큰 재발급·회전, Host·MCP 설정 수정, CLI 명령 실행, 업무 생성·수락·갱신·완료를 setup 과정에서 하지 않는다.

진단 요약에는 허용 목록의 상태값, 오류 code·scope·outcome·requestId만 넣는다. 브로커 URL·로컬 토큰·파일 경로는 넣지 않는다. requestId는 브로커가 준 문자열이므로 `[A-Za-z0-9._:-]` 1~128자일 때만 그대로 넣고, 아니면 `invalid`로 바꾼다(모델에 보이는 메시지로 임의 텍스트가 들어가지 않게 한다).

### 예시

`prompts/list` 항목:

```json
{"name":"devlink_setup","title":"DevLink setup","description":"Diagnose this DevLink connection once (read-only) and list only the setup steps still needed. Nothing is registered, created, rotated or executed until you choose a step."}
```

검증된 Agent가 있고 Host 세션이 아직 바인딩되지 않은 경우의 `prompts/get` 메시지 본문:

```text
DevLink setup guide (devlink_setup). The connection was diagnosed once, read-only; nothing was registered, created or changed.

Diagnosis:
- broker running, central Connected, identity verified, credentials valid, hostSession unbound

Needed steps, in order:
1. [register_agent] (assistant, only when you ask) Bind this Host session to your existing DevLink Agent. On request the assistant first calls get_my_context({}) and shows the Agent name, tool and project; only after you confirm that Agent does it call register_agent({}) with an empty object. This never creates an Agent or issues a token.

Rules for this reply and the rest of the setup:
- Present the diagnosis and the numbered steps to the user in their language, then ask which step they want to take. Do not call any tool in this reply.
- Call get_my_context({}) or register_agent({}) only after the user explicitly asks for that step in this conversation. register_agent takes {} only.
- Never ask for, accept or repeat passwords, tokens or credential file contents.
- Never create an Agent, re-issue or rotate a token, edit Host or MCP settings, run CLI commands, or create, accept, update or complete tasks as part of setup. Steps marked "you run" are for the user to run in their own terminal.
```

## 검증

### SDK 회귀 (`test/setup-prompt.test.mjs`, 기본 `npm test` 포함)

- `prompts/list`에 `devlink_setup`만 있고 인자가 없다. 목록·도구 조회는 브로커 요청 0회
- 미바인딩 → `register_agent` 단계만 표시(`review_mcp_config` 제외), 브로커 요청은 본문 없는 `GET /v1/connection` 1회, 토큰·브로커 주소 미노출. 처리 뒤 BrokerClient는 `inactive`, 대기 요청 0. 이어서 다른 Agent의 문맥을 읽어도 오류가 없다(문맥을 고정하지 않음)
- 문맥 모순으로 차단된 MCP 프로세스 → `clientIssue: BROKER_CONTEXT_INVALID`를 보여 주고 `inspect_profile_then_rebind`, `open_or_restart_host`만 표시
- 허용 문자 밖의 requestId → `invalid`로 대체, 원문 미포함
- 바인딩 완료 → 단계 없음
- `CENTRAL_UNAUTHENTICATED`·세션 충돌·재연결·inbox 실패 → 각자의 안내만 표시, 등록 단계 없음
- 자격 파일 없음 → `prepare_cli_profile`, `open_or_restart_host`, 브로커 요청 0회
- `clientIssue`가 있으면 bound여도 준비 완료가 아님, 단계가 비면 `retry_diagnosis`
- 14개 `nextAction` 전부 안내문이 있고, `register_agent`만 assistant 실행

### 실제 Host 회귀 (`npm run test:setup-prompt`)

`scripts/verify-setup-prompt.mjs`는 빌드한 `dist/index.js`를 실제 Host가 stdio로 실행하게 하고, 합성 HTTP 브로커(`test/helpers/fake-broker.mjs` + `/v1/connection`·`/v1/context` 훅)에 연결한다. Host 설정은 `verify-hosts.mjs`의 `hostConfiguration`을 그대로 쓰므로 사용자 설정을 바꾸지 않는다. 자격 파일은 private DACL 임시 디렉터리에 두고, 출력에 토큰이 없는지와 종료 후 MCP 자식 프로세스가 남지 않았는지 확인한다. 실제 모델 호출 비용이 들어 기본 `npm test`에는 넣지 않는다.

2026-11-09 실행 결과(`--host both`, 통과):

| Host | 단계 | 실제 관찰 |
| --- | --- | --- |
| Claude Code 2.1.319, `claude-opus-5-5`/high | 1턴 `/mcp__devlink__devlink_setup` | init `slash_commands`에 `mcp__devlink__devlink_setup` 있음, 모델 도구 목록에는 없음. 도구 호출 0회. 브로커 `GET /v1/connection` 1회. 답변은 진단과 등록 단계 1개를 보여 주고 사용자 선택을 물음 |
| | 2턴 "I choose step 1." (선택만, 확인 없음) | `get_my_context({})`만 호출(`GET /v1/context`). Agent 이름·ID·도구·프로젝트를 보여 주고 이 Agent가 맞는지 물음. `register_agent` 호출 없음. 사용자 문장에 지시가 없으므로 prompt 규칙이 등록 전에 멈추게 한 것이다 |
| | 3턴 "그 Agent가 맞다. 연결하라" | `register_agent({})`만 호출. 브로커 `POST /v1/sessions`(recover) → `POST /v1/agents/register` → bind |
| | 4턴 prompt 재실행 | 도구 호출 0회, `GET /v1/connection` 1회. 답변에 `register_agent(`·`[register_agent]` 없음(단언). 답변은 "남은 단계 없음"이었다(아티팩트 확인) |
| | 자격 파일 없음 | 도구 호출 0회, 브로커 요청 0회, 답변이 `devlink setup` 실행을 안내 |
| Codex 0.167.0 | prompt 없이 도구 흐름 | `get_connection_status({})` → `get_my_context({})` → `register_agent({})` 각 1회 성공, `hostSession` unbound 확인 후 Agent `13` 바인딩. 브로커 요청 `GET /v1/connection` → `GET /v1/context` → `POST /v1/sessions` → `POST /v1/agents/register` → bind |

Claude 다중 턴은 `--input-format stream-json`으로 같은 프로세스에 사용자 메시지를 차례로 보냈다. 앞 턴의 `result` 이벤트를 받은 뒤 다음 메시지를 보낸다. 도구 실행 여부는 모델 문장이 아니라 Host의 `tool_use`/`tool_result` 이벤트와 브로커 도착 기록으로 판정한다. 1턴 답변의 `register|bind` 일치와 4턴의 등록 안내 부재는 보조 확인이다. 실제 중앙 서비스·Rust 브로커는 이 회귀 범위가 아니다(`actualCentralVerified: false`).

### 교차검증

Claude Code CLI, 실제 init 모델 `claude-opus-5-5`, effort `high`, 읽기 전용 도구(Read·Grep·Glob)로 검토했다.

- 1차: must-fix 없음. should-fix 4건(재인증 안내에 브로커 재시작 누락, 선택과 확인을 한 메시지로 보낸 Host 회귀, stdin 오류 미처리, prompt 처리 중 상태 불변 테스트 부재)과 nit 4건(약한 문장 단언, `--version` 실패 처리, 재바인딩 명령 누락, requestId 원문 노출)을 모두 반영했다.
- 2차: must-fix 없음. should-fix 1건(`agent use`는 멈춘 브로커가 필요하고 Agent 토큰을 다시 요구하는데 재바인딩 안내가 이를 빠뜨림)과 nit 4건(2턴 사용자 문장이 이미 "먼저 보여 달라"를 지시함, 4턴 번호 목록 단언 불안정, spawn 실패 시 마감까지 대기, 임시 디렉터리 생성 실패 처리)을 반영했다. 반영 후 2턴을 "I choose step 1."만 보내도록 바꿔 실제 Host 회귀를 다시 통과했다.
- 3차: must-fix 없음. 2차 반영분(2턴 선택만 전송, 4턴 단언, spawn 실패 처리, 임시 디렉터리 처리)은 결함 없음으로 확인했다. should-fix 1건(`agent use`로 다른 Agent를 고르면 `--project`가 필요함)을 반영해 안내를 `devlink agent use --id <ID> --project <PATH>`로 고쳤다. nit 1건(기존 `privateDirectory`의 DACL 실패 시 임시 디렉터리 잔존)은 이번 변경 범위 밖이라 남겼다.

## 미검증 항목

- Claude Code 대화형 TUI의 `/` 자동완성 목록에 prompt가 표시되는지(PTY 자동화 도구 없음, 수동 확인 필요)
- 대화형 Claude Code에서 세션이 열린 뒤 직접 입력한 `/mcp__devlink__devlink_setup`의 실행(비대화형 `-p`와 stream-json 다중 턴으로만 확인)
- Codex 데스크톱 앱·IDE 확장의 MCP prompt 처리(CLI·앱 서버 바인딩만 확인)
- 실제 Rust 브로커·중앙 서비스에 연결한 상태의 prompt 실행. 진단 응답 형식은 Rust `/v1/connection` 계약과 같은 Zod 스키마로 검증하지만, 실제 연결 실행은 하지 않았다
- `--disable-slash-commands`로 실행한 Claude Code에서는 prompt 슬래시 명령도 쓸 수 없을 것으로 추정한다(미확인). 이때는 도구 흐름을 쓴다
- 자격 파일이 없을 때 진단 `issue` 코드가 `CREDENTIAL_FILE_INVALID`로 나와, 모델이 "파일이 거부됨"으로 설명한 사례가 있었다. 기존 진단 동작이며 이번 범위에서 바꾸지 않았다

## 재현 메모

- probe 서버와 로그는 세션 임시 디렉터리에 두었고 저장소에 커밋하지 않는다. 다시 확인할 때는 위 등록 내용으로 SDK probe 서버를 만들고, 수신 메서드 로그에서 `prompts/list`·`prompts/get` 유무를 본다.
- 실제 모델 호출은 사용량을 소모하므로 기본 `npm test`에 넣지 않는다.
