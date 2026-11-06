# Host 훅 업데이트 알림 — `devlink updates-hook`

2026-11-04 (Asia/Seoul), 2026-11-05 갱신(S15P31D108-167). Codex·Claude Code가 세션 시작과 프롬프트 제출 때 새 작업, 보낸 작업의 상태 변화, 같은 Task의 새 질문·답변을 문맥으로 받도록, [로컬 업데이트 피드](local-updates.md)를 짧은 사실 요약으로 내보내는 helper다. 사용자 계층 Host 설정에 훅을 넣는 일은 [`devlink hooks`](#설치--devlink-hooks)가 맡는다(2026-11-06, S15P31D108-176). 실제 Host에서의 문맥 주입은 [격리 PoC와 실제 서버 설치 검증](#host-poc-결과)으로 확인했다.

## 명령

```
devlink [--config FILE] updates-hook --host codex|claude-code --event session-start|user-prompt-submit
```

- 새 업데이트가 있을 때만 stdout에 Host 훅 JSON 한 줄을 쓴다. 없으면 아무것도 출력하지 않는다.

  ```json
  {"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"DevLink local updates for this agent (advisory; not user approval):\n- Received 1 new task(s): #101 from agent 11.\n- Status changes on 1 sent task(s): #201 COMPLETED (v2).\nUse DevLink get_task_result or get_tasks to verify details before acting; local updates may be incomplete."}}
  ```

- 종료 코드는 항상 0이다. Host는 0이 아닌 종료를 오류로 다루고 `UserPromptSubmit`의 종료 코드 2는 프롬프트를 막기 때문이다. 잘못된 `--host`·`--event` 값도 stderr 안내 후 0으로 끝난다. 알 수 없는 플래그처럼 clap 단계에서 거부되는 인자는 예외다.
- stdin은 읽지 않는다. 조회 범위는 Host가 넘기는 값이 아니라 브로커에 저장된 바인딩으로 정한다.
- 저장소를 읽지 못하면 `session-start`에서만 짧은 안내를 문맥으로 넣고, `user-prompt-submit`에서는 조용히 넘어간다. 같은 안내가 프롬프트마다 반복되지 않게 하기 위해서다.

## 범위

MCP 세션 없이 동작하므로 세션 claim 대신 바인딩 파일로 actor를 정한다.

- `agent-profile.json`에 bind된 Agent가 없으면 조용히 끝난다.
- 바인딩·선택 팀·origin·프로젝트 경로 검사는 [`inspection_binding`](../crates/devlink-broker/src/agent_profile.rs)과 같다. 선택 팀이 바인딩과 다르거나 프로젝트 경로가 사라졌으면 오류다.
- 현재 작업 디렉터리가 bind된 프로젝트 루트 안에 없으면 조용히 끝난다. 사용자 전역 훅으로 설치해도 다른 프로젝트에 이 Agent의 작업이 섞이지 않는다.
- 피드는 `get_updates`와 같다(`Feed::load`). 현재 origin·Agent·팀의 받은 작업, 보낸 작업 알림, 질문·답변 도착 정보만 포함하고 중앙은 호출하지 않는다.

## cursor와 동시성

- Host마다 `.devlink-runtime/updates-<host>.json`(`{"schemaVersion":1,"cursor":"v2...."}`)에 별도 cursor를 둔다. 이전에 저장한 v1 cursor도 그대로 이어 읽는다. Codex가 읽은 업데이트도 Claude Code에는 다시 나온다. MCP `get_updates`의 cursor와도 독립이다.
- 실행마다 남은 항목을 모두 소비한다. 수신함·알림은 각각 1024건, 질문·답변은 4096건으로 제한되므로 한 번에 읽어도 된다. 질문·답변이 상한에 닿아 오래된 절반을 정리하면 cursor가 맞지 않아 위의 재시작 안내와 함께 남은 항목을 다시 나열한다.
- cursor를 먼저 저장한 뒤 요약을 출력한다. Host가 출력을 버리면 그 요약은 다시 나오지 않으며, 이때는 `get_updates`·`get_tasks`로 확인한다. 중복보다 누락을 택한 것은 같은 알림이 프롬프트마다 반복되는 것을 막기 위해서다.
- `updates-<host>.lock`을 기다리지 않고 시도한다. 같은 Host의 다른 세션이 동시에 실행 중이면 이번 실행은 조용히 건너뛰고 다음 실행이 이어받는다. 같은 Host의 여러 세션은 cursor를 공유하므로 업데이트는 먼저 실행된 세션 하나에만 나온다.
- 저장소가 정리·교체되어 cursor가 맞지 않거나 cursor 파일을 해석할 수 없으면 처음부터 다시 나열하고 요약에 `Local update history was cleared or replaced`를 넣는다.

## 요약 내용

- 사실 서술만 넣는다. 받은 작업은 task ID와 요청 Agent ID, 보낸 작업은 task별 가장 높은 version의 상태, 질문·답변은 task별 새 질문·답변 개수다(예: `- New questions/answers from the other agent on 1 task(s): #300 (1 question, 2 answers). Read them with get_task_result; answer only when the user wants you to.`).
- 보낸 작업의 최신 version은 이번 실행분이 아니라 보존된 전체 알림 이력에서 고른다. 그 최신 version이 이번에 새로 소비한 이벤트일 때만 보고한다. 이전 실행에서 이미 보고한 것보다 낮은 version이 늦게 도착하면 cursor는 넘기되 요약하지 않으므로, 보고한 상태가 뒤로 돌아가지 않는다.
- 범주마다 task ID는 최대 10개이고 나머지는 `and N more`로 센다.
- prompt·resultText·질문·답변 본문·파일 경로는 넣지 않는다. 다른 Agent가 쓴 본문이 Host 문맥에 지시처럼 들어가지 않도록 하기 위해서다.
- 업데이트는 사용자 승인이 아니라는 문장과 `get_task_result`·`get_tasks`로 확인하라는 안내를 항상 붙인다.

## 설치 — `devlink hooks`

```
devlink [--config FILE] hooks status    --host codex|claude-code
devlink [--config FILE] hooks install   --host codex|claude-code [--yes] [--replace]
devlink [--config FILE] hooks uninstall --host codex|claude-code [--yes]
```

- 대상은 사용자 계층 파일이다.
  - Claude Code: `$CLAUDE_CONFIG_DIR/settings.json`, 없으면 `~/.claude/settings.json`
  - Codex: `$CODEX_HOME/hooks.json`, 없으면 `~/.codex/hooks.json`
  - helper가 bind된 프로젝트 밖에서는 아무것도 출력하지 않으므로 사용자 계층 하나로 충분하다. 실제 서버 검증도 이 위치에서 했다(아래 "실제 서버 설치 검증").
- `install`과 `uninstall`은 `--yes`가 없으면 계획(JSON)만 출력하고 파일을 쓰지 않는다. `--yes` 실행은 그 시점에 계획을 다시 세운다. 따라서 아래의 digest 확인은 같은 실행 안에서 계획하고 쓰는 사이의 변경만 막는다.
- `status`는 설정 파일이 있으면 이 실행 파일과 프로필로 만든 명령과 비교해 결과를 보고한다.
  - `installed`: 두 이벤트 모두 기대한 명령이 있다.
  - `stale`: 두 이벤트 모두 DevLink 항목은 있지만, 적어도 하나가 기대한 명령이 아니다(경로나 프로필이 다름). 이때 `install`은 충돌로 멈춘다.
  - `partial`, `missing`: 일부 이벤트에만 있거나 없다.
  - 설정 파일이 없거나 경로 때문에 명령을 만들 수 없으면(`HOOKS_PATH_UNSUPPORTED`, 직접 넣은 훅) DevLink 항목이 있는지만 본다.
- Host 파일이 JSON이 아니거나, 키가 중복되거나, hooks 형식이 예상과 다르면 `HOOKS_CONFIG_INVALID`(파일 경로 포함)로 멈추고 아무것도 쓰지 않는다.
- 명령은 `<devlink 절대 경로> --config <설정 절대 경로> updates-hook --host <host> --event <event>`로 `SessionStart`·`UserPromptSubmit`에 넣는다. timeout은 10초다.
  - 경로는 따옴표 없이 `/`로 쓴다. Host마다 훅을 실행하는 셸(bash·cmd·PowerShell)이 달라도 동작하던 형식이다.
  - 두 경로는 canonicalize한 값을 쓴다. 그래서 같은 프로필이면 대소문자나 상대 경로 표기와 관계없이 같은 명령이 된다.
  - 다음 경로는 셸별 규칙을 추측하지 않고 `HOOKS_PATH_UNSUPPORTED`로 거부한다. 이때는 아래 예시로 직접 넣는다.
    - 공백이나 셸 특수문자(`\` 포함)가 있는 경로
    - UNC 경로(`//server/...`)
  - 비 ASCII 경로(예: 한글 사용자 폴더)는 허용하지만 실제 Host에서 확인하지 않았다.
- 병합 규칙:
  - 다른 키와 다른 훅, 다른 Host의 DevLink 항목은 그대로 둔다. 중복 키나 형식이 맞지 않는 파일은 고치지 않고 거부한다.
  - DevLink 항목은 command에 `updates-hook --host <host>`가 들어 있는 것으로만 식별한다. 시작 래퍼처럼 이 문구가 없는 명령은 DevLink 항목으로 보지 않는다.
  - 같은 명령이 이미 있으면 `unchanged`다. 경로나 프로필이 다른 DevLink 항목이 있으면 충돌로 멈추고, `plan.existing`에 기존 명령을 보여준다. `--replace`를 줄 때만 바꾼다.
  - `uninstall`은 해당 Host의 DevLink 항목만 지우고, 비게 된 그룹·이벤트만 정리한다.
  - 바꿀 필요가 없는 이벤트와 다른 키는 원래 JSON 텍스트를 그대로 둔다. 다시 쓰는 이벤트 배열 안에서는 다른 그룹·훅의 값이 보존되지만, 키 순서와 숫자 표기는 정규화될 수 있다.
  - 새 DevLink 항목은 matcher 없는 그룹으로 추가하므로 모든 시작 경로(startup·resume 등)에서 실행된다. 기존 matcher 그룹 안의 다른 훅은 그대로 남는다.
  - 결과가 1 MiB를 넘으면 쓰지 않는다(`HOOKS_CONFIG_INVALID`).
  - `hooks` 객체는 파일의 두 칸 들여쓰기에 맞춰 다시 조립한다. 보존하는 이벤트는 원래 텍스트를 그대로 둔다.
- 쓰기는 MCP 설정 제공자와 같은 방식이다.
  - `~/.devlink-mcp-config/<대상 해시>/apply.lock` 잠금을 잡는다.
  - 원본은 `backups/`에 비공개 백업으로 남긴다.
  - 계획 때와 내용이 바뀌었으면 쓰지 않는다(digest 비교).
  - 원자적으로 교체한다.
- 적용하면 `nextActions`에 `restart_host`를 넣는다. Codex에는 `trust_codex_hooks`도 넣는다. Codex는 새로 넣거나 바뀐 훅을 대화형 `/hooks`에서 한 번 신뢰 승인해야 실행한다.
- setup은 MCP 설정 단계 뒤에 같은 사용자 계층 파일을 읽기만 한다. 이 실행 파일·프로필로 만든 명령이 두 이벤트에 모두 있지 않으면(`status`가 `installed`가 아니면) `nextActions`에 `install_host_hooks`를 넣는다. 경로 때문에 명령을 만들 수 없으면 DevLink 항목이 있는지만 본다. `setup status`는 이 판정을 하지 않는다. setup이 이 파일을 쓰지는 않는다.

## Host 설정 예시 (직접 설치)

`devlink`이 PATH에 있다고 가정한다. 기본 위치가 아닌 설정을 쓰면 `--config`를 붙인다. timeout은 짧게 둔다. helper는 로컬 파일만 읽는다.

Claude Code (`.claude/settings.json` 또는 `~/.claude/settings.json`):

```json
{
  "hooks": {
    "SessionStart": [{"hooks": [{"type": "command", "command": "devlink updates-hook --host claude-code --event session-start", "timeout": 10}]}],
    "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "devlink updates-hook --host claude-code --event user-prompt-submit", "timeout": 10}]}]
  }
}
```

Codex는 사용자 계층 `~/.codex/hooks.json`과 프로젝트 계층 `<repo>/.codex/hooks.json` 중 하나에 둔다. 어느 쪽이든 처음 실행할 때 대화형 Codex의 `/hooks`에서 훅을 신뢰 승인해야 하며, 명령이 바뀌면 다시 승인해야 한다.

- 사용자 계층: 프로젝트마다 신뢰할 필요가 없다. helper는 작업 디렉터리가 bind된 프로젝트 밖이면 출력하지 않으므로 다른 프로젝트에 섞이지 않는다.
- 프로젝트 계층: 그 프로젝트가 신뢰된 경우에만 읽힌다. 신뢰는 Codex의 프로젝트 신뢰 확인으로 `~/.codex/config.toml`의 `[projects.'<경로>'] trust_level = "trusted"`에 저장된다.
- `-c`로 신뢰를 넘길 때 주의: Codex 0.166.0의 `-c` 키는 `.`으로만 나누고 TOML 따옴표를 해석하지 않는다(`codex-rs/config/src/overrides.rs`). `projects.'<경로>'.trust_level`처럼 따옴표를 붙이면 따옴표가 키에 포함되어 신뢰가 적용되지 않는다. 경로에 마침표가 있으면 `-c`로는 신뢰를 넘길 수 없다.

Codex (`~/.codex/hooks.json` 또는 `<repo>/.codex/hooks.json`):

```json
{
  "hooks": {
    "SessionStart": [{"hooks": [{"type": "command", "command": "devlink updates-hook --host codex --event session-start", "timeout": 10}]}],
    "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "devlink updates-hook --host codex --event user-prompt-submit", "timeout": 10}]}]
  }
}
```

## 검증

- `cargo test -p devlink-broker --test update_hook`: task별 질문·답변 개수와 다른 Agent 제외, 한 번만 보고, task별 최신 상태, 호출 사이에 늦게 도착한 낮은 version 무보고, 다른 팀·Agent·요청자 제외, 본문 미포함, Host별 cursor, 10개 상한, 미바인딩·프로젝트 밖·잠금 경합 시 소비 없이 무출력, 저장소 정리·손상 cursor 재시작 안내, 선택 팀 불일치와 잘못된 consumer 오류.
- `update_hook::tests`: consumer 이름 제한과 목록 상한.
- `cargo test -p devlink-cli --test cli_updates_hook`: 항상 종료 코드 0, 업데이트 없을 때 무출력, 잘못된 값의 stderr 안내, 저장소 오류는 session-start에서만 안내.
- `mcp_config::hooks::tests`:
  - 명령 형식(슬래시, 따옴표가 필요한 경로 거부)
  - 생성·병합·재사용 시 다른 키·훅·큰 정수 보존, 정확한 백업
  - 다른 DevLink 항목 충돌과 `--replace`, 같은 그룹의 다른 훅과 다른 Host 항목 보존
  - uninstall 범위
  - 손상·중복 키·잘못된 형식 거부, 계획 뒤 외부 수정 거부
- `cargo test -p devlink-cli --test cli_hooks`: `CLAUDE_CONFIG_DIR`·`CODEX_HOME`을 격리한 상태에서 확인한다.
  - 계획만 출력하고 쓰지 않음, `--yes` 적용과 재사용
  - Codex 신뢰 안내
  - 충돌 시 원본 그대로, `--replace` 백업, uninstall 뒤 래퍼 훅 보존
  - 설정 파일이 없으면 거부

## Host PoC 결과

### 격리 합성 PoC

2026-11-04, develop `6e34434`, Windows 11. 임시 `--config`에 bind된 Agent와 합성 수신·알림 이벤트를 저장하고 비대화형으로 실행했다. 사용자 전역 DevLink·Claude·Codex 설정과 중앙 서버는 쓰지 않았다.

| Host | 실행 방식 | 결과 |
| --- | --- | --- |
| Claude Code 2.1.310 | 프로젝트 `.claude/settings.json`, `claude -p --setting-sources project` | SessionStart·UserPromptSubmit 모두 요약이 모델 문맥에 들어감. 새 이벤트만 표시하고, 새 이벤트가 없으면 출력 없음 |
| Codex 0.166.0 | `codex exec --dangerously-bypass-hook-trust`, `-c hooks.<Event>=[...]` | SessionStart·UserPromptSubmit 모두 실행됨. 모델이 task ID·Agent·상태를 정확히 요약함. 원문 복사 요청은 developer 메시지라는 이유로 거절 |
| Codex 0.166.0 | 같은 명령, 프로젝트 `.codex/hooks.json`, `-c projects.'<경로>'.trust_level="trusted"` | 훅 미실행(cursor 미생성). PoC 명령 문제: 따옴표가 키에 포함되어 프로젝트가 신뢰되지 않았다 |

- 두 모델 모두 요약을 사용자 승인으로 해석하지 않았다.
- prompt·resultText에 넣은 표식 문자열은 출력되지 않았고, Host별 cursor는 서로 독립이었다.
- 프로젝트 계층 미실행 원인 확인: 훅 없이 `.codex/config.toml`에 `model_reasoning_effort = "low"`만 둔 폴더로 비교했다. 따옴표 있는 키(`projects.'<경로>'…`)는 실행 헤더가 사용자 기본값 `xhigh`로 남아 계층이 로드되지 않았다. 따옴표 없는 키(`projects.<경로>…`)는 `low`로 바뀌어 계층이 로드됐다.

### 실제 서버 설치 검증

2026-11-04, client `667a443` 빌드, Windows 11, Codex 0.166.0, Claude Code 2.1.310. 개발 PC 한 대에 CLI·MCP 서버와 두 Host의 사용자 계층 훅(`~/.codex/hooks.json`, `~/.claude/settings.json`)을 설치했다. 실제 중앙 서버(`wss://api.devlink.ai.kr`)에서 본인 Agent가 본인에게 보낸 설치 확인용 작업 2건으로 검증했다.

| 확인 | 결과 |
| --- | --- |
| Codex 훅 신뢰 | 대화형에서 두 훅을 신뢰 처리. 훅 목록에서 둘 다 trusted, 우회 플래그 없이 실행 |
| Codex SessionStart·UserPromptSubmit | 중앙에서 받은 작업의 수신과 COMPLETED(v3) 알림을 시작 문맥과 다음 프롬프트 문맥에서 각각 읽음 |
| Claude Code SessionStart·UserPromptSubmit | 같은 수신·완료 알림을 도구 조회 없이 문맥에서 읽음 |
| 프로젝트 범위 | 프로젝트 밖에서 실행하면 출력 없음 |
| 승인 해석 | 두 모델 모두 알림을 승인으로 보지 않았고 작업 생성·변경 도구를 호출하지 않음 |

- 이 설치는 SessionStart에 `updates-hook`을 직접 걸지 않았다. 대신 연결 안내(기존 Agent 확인·`register_agent`)와 로컬 업데이트를 함께 출력하는 시작 래퍼를 썼다. 래퍼는 이 저장소에 포함되지 않았다. UserPromptSubmit은 `updates-hook`을 직접 호출했다.
- 두 Host가 Agent 하나와 브로커 하나를 공유했다. 알림 cursor는 Host별로 독립이지만, MCP 업무 세션 소유권은 한 번에 한 Host만 갖는다. 소유 Host 종료 직후 다른 Host는 `SESSION_CONFLICT`를 받고 임대 만료(최대 약 90초)를 기다려야 했다.
- 상세 기록은 팀 Notion "현재 PC DevLink 자동 알림 설치·검증 기록"(2026-11-04)에 있다.

### 남은 미검증

- 신뢰된 프로젝트에서 프로젝트 계층 `.codex/hooks.json` 훅이 실행되는지. 두 검증 모두 사용자 계층 또는 `-c`만 사용했다.
- 대화형 세션(TUI)에서 시작·재개·입력 시점별 동작을 따로 기록하지 않았다.
- 시작 래퍼 없이 SessionStart에 `updates-hook`만 건 구성의 실제 서버 검증. 격리 PoC에서는 이 구성으로 확인했다.
