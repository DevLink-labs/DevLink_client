# Host 훅 업데이트 알림 — `devlink updates-hook`

2026-11-04 (Asia/Seoul). Codex·Claude Code가 세션 시작과 프롬프트 제출 때 새 작업과 보낸 작업의 상태 변화를 문맥으로 받도록, [로컬 업데이트 피드](local-updates.md)를 짧은 사실 요약으로 내보내는 helper다. Host 설정 파일에 훅을 설치하는 일은 범위 밖이다. 실제 Host에서의 문맥 주입은 [격리 PoC](#host-poc-결과)로 확인했다.

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
- 피드는 `get_updates`와 같다(`Feed::load`). 현재 origin·Agent·팀의 받은 작업과 보낸 작업 알림만 포함하고 중앙은 호출하지 않는다.

## cursor와 동시성

- Host마다 `.devlink-runtime/updates-<host>.json`(`{"schemaVersion":1,"cursor":"v1...."}`)에 별도 cursor를 둔다. Codex가 읽은 업데이트도 Claude Code에는 다시 나온다. MCP `get_updates`의 cursor와도 독립이다.
- 실행마다 남은 항목을 모두 소비한다. 두 저장소는 각각 1024건으로 제한되므로 한 번에 읽어도 된다.
- cursor를 먼저 저장한 뒤 요약을 출력한다. Host가 출력을 버리면 그 요약은 다시 나오지 않으며, 이때는 `get_updates`·`get_tasks`로 확인한다. 중복보다 누락을 택한 것은 같은 알림이 프롬프트마다 반복되는 것을 막기 위해서다.
- `updates-<host>.lock`을 기다리지 않고 시도한다. 같은 Host의 다른 세션이 동시에 실행 중이면 이번 실행은 조용히 건너뛰고 다음 실행이 이어받는다. 같은 Host의 여러 세션은 cursor를 공유하므로 업데이트는 먼저 실행된 세션 하나에만 나온다.
- 저장소가 정리·교체되어 cursor가 맞지 않거나 cursor 파일을 해석할 수 없으면 처음부터 다시 나열하고 요약에 `Local update history was cleared or replaced`를 넣는다.

## 요약 내용

- 사실 서술만 넣는다. 받은 작업은 task ID와 요청 Agent ID, 보낸 작업은 task별 가장 높은 version의 상태다.
- 보낸 작업의 최신 version은 이번 실행분이 아니라 보존된 전체 알림 이력에서 고른다. 그 최신 version이 이번에 새로 소비한 이벤트일 때만 보고한다. 이전 실행에서 이미 보고한 것보다 낮은 version이 늦게 도착하면 cursor는 넘기되 요약하지 않으므로, 보고한 상태가 뒤로 돌아가지 않는다.
- 범주마다 task ID는 최대 10개이고 나머지는 `and N more`로 센다.
- prompt·resultText·파일 경로는 넣지 않는다. 다른 Agent가 쓴 본문이 Host 문맥에 지시처럼 들어가지 않도록 하기 위해서다.
- 업데이트는 사용자 승인이 아니라는 문장과 `get_task_result`·`get_tasks`로 확인하라는 안내를 항상 붙인다.

## Host 설정 예시

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

Codex는 사용자 계층 `~/.codex/hooks.json`에 둔다. 처음 실행할 때 대화형 Codex의 `/hooks`에서 훅을 신뢰 승인해야 하며, 명령이 바뀌면 다시 승인해야 한다. 사용자 계층에 두어도 helper는 작업 디렉터리가 bind된 프로젝트 밖이면 출력하지 않으므로 다른 프로젝트에 섞이지 않는다.

프로젝트 계층 `<repo>/.codex/hooks.json`은 권장하지 않는다. Codex는 프로젝트 `.codex/`를 그 프로젝트가 신뢰된 경우에만 읽는데, 아래 PoC에서는 `-c projects.'<경로>'.trust_level="trusted"`와 `git init`을 적용해도 이 파일이 로드되지 않았다. 원인은 확정하지 못했다.

Codex (`~/.codex/hooks.json`):

```json
{
  "hooks": {
    "SessionStart": [{"hooks": [{"type": "command", "command": "devlink updates-hook --host codex --event session-start", "timeout": 10}]}],
    "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "devlink updates-hook --host codex --event user-prompt-submit", "timeout": 10}]}]
  }
}
```

## 검증

- `cargo test -p devlink-broker --test update_hook`: 한 번만 보고, task별 최신 상태, 호출 사이에 늦게 도착한 낮은 version 무보고, 다른 팀·Agent·요청자 제외, 본문 미포함, Host별 cursor, 10개 상한, 미바인딩·프로젝트 밖·잠금 경합 시 소비 없이 무출력, 저장소 정리·손상 cursor 재시작 안내, 선택 팀 불일치와 잘못된 consumer 오류.
- `update_hook::tests`: consumer 이름 제한과 목록 상한.
- `cargo test -p devlink-cli --test cli_updates_hook`: 항상 종료 코드 0, 업데이트 없을 때 무출력, 잘못된 값의 stderr 안내, 저장소 오류는 session-start에서만 안내.

## Host PoC 결과

2026-11-04, develop `6e34434`, Windows 11. 임시 `--config`에 bind된 Agent와 합성 수신·알림 이벤트를 저장하고 비대화형으로 실행했다. 사용자 전역 DevLink·Claude·Codex 설정과 중앙 서버는 쓰지 않았다.

| Host | 실행 방식 | 결과 |
| --- | --- | --- |
| Claude Code 2.1.310 | 프로젝트 `.claude/settings.json`, `claude -p --setting-sources project` | SessionStart·UserPromptSubmit 모두 요약이 모델 문맥에 들어감. 새 이벤트만 표시하고, 새 이벤트가 없으면 출력 없음 |
| Codex 0.166.0 | `codex exec --dangerously-bypass-hook-trust`, `-c hooks.<Event>=[...]` | SessionStart·UserPromptSubmit 모두 실행됨. 모델이 task ID·Agent·상태를 정확히 요약함. 원문 복사 요청은 developer 메시지라는 이유로 거절 |
| Codex 0.166.0 | 같은 명령, 프로젝트 `.codex/hooks.json` | 훅 미실행(cursor 미생성) |

- 두 모델 모두 요약을 사용자 승인으로 해석하지 않았다.
- prompt·resultText에 넣은 표식 문자열은 출력되지 않았고, Host별 cursor는 서로 독립이었다.
- 미검증: `~/.codex/hooks.json` 파일 자체(PoC는 같은 비프로젝트 계층인 `-c` 지정만 확인), 대화형 세션(TUI)의 시작·재개·입력, 우회 플래그 없이 저장된 훅 신뢰로 실행하는 경우, 실제 중앙 서버에서 받은 이벤트.
