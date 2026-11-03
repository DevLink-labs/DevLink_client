# DevLink MCP 실행 기반

S15P31D108-59의 TypeScript·Node.js MCP 실행 기반, -60 로컬 브로커 호출 계층, -61 공통 등록 구조와 7개 업무 도구, -62 인증 컨텍스트 처리입니다. MCP Host가 stdio 자식 프로세스로 실행합니다. 초기화·목록·`devlink_ping`은 브로커 없이 동작하며, 업무 도구 요청부터 브로커 세션을 활성화합니다.

브로커 호출 계층의 [HTTP·세션 계약](docs/broker-contract-v0.1.md)은 먼저 복구한 인스턴스를 인정하는 정책을 설명합니다. S15P31D108-97에서 실제 SDK→Rust→격리 중앙의 Agent 등록과 양방향 컨텍스트·결과 전달을 검증했습니다. [중앙 계약·재현 방법](docs/central-roundtrip.md)에서 실행 범위와 한계를 확인할 수 있습니다.

[인증 컨텍스트·프로필 자격 파일](docs/agent-context.md)에 -62의 처리 정책을, [공유 계약](../../contracts/local-broker/v1/README.md)에 JSON Schema와 fixture를 정리했습니다. 현재 Rust는 CLI 프로필 자격 발행, 세션 API와 7개 업무 도구의 중계를 제공합니다. Agent 생성은 CLI, MCP register_agent는 기존 Agent 확인·세션 연결입니다.

[공통 오류·시간 제한 계약](docs/error-contract.md)은 -63의 오류 코드·원본 요청 식별자·requestKey 보존, 저장/전달 결과와 전체 호출 예산을 설명합니다. SDK의 입력 검증과 프로토콜 오류는 원래 wire id를 유지합니다.

[도구 입력·결과 예제와 확장 방법](docs/tool-contracts-v0.1.md)에 중앙/Rust 계약에 맞춘 7개 도구 JSON을 정리했습니다. 상태·완료에는 최신 version이 필수이며 완료는 resultText/taskFiles를 사용합니다. 중앙의 별도 verification 필드는 없으므로 검증 내용은 resultText에 명시합니다.

[요청·수락·결과 등록 지침](docs/agent-workflow.md)은 -65의 사용자 지정 수신자, 요청 템플릿, 명시적 수락 후 로컬 작업, 완료·실패·결과 불명 처리 순서를 설명합니다. [Codex](docs/hosts/codex.md)·[Claude Code](docs/hosts/claude-code.md)용 붙여 넣을 지침과 [스키마 검증 가능한 JSON 예제](examples/workflow.json)를 함께 제공합니다.

[MVP 통합 검증 절차](docs/mvp-integration.md)는 -66의 실제 Host 검증 증거 양식을 제공합니다. `integration:check`는 차단 사유 확인용이며 실제 MVP 통과를 판정하지 않습니다. -97의 실제 중앙 두 사용자 SDK 왕복과 LLM Host의 자율 코드 수정 검증은 구분합니다.

## 설치와 실행

Node.js 24 이상이 필요합니다. 현재 Windows, Node.js 24.18.0, npm 12.0.2에서 검증했습니다. 아래 명령은 저장소 루트에서 실행합니다.

```powershell
npm --prefix packages/mcp-server ci
npm --prefix packages/mcp-server run build
node packages/mcp-server/dist/index.js --transport stdio
```

서버는 stdin에서 MCP 요청을 기다립니다. 명령이 시작됐다는 사실만으로 Host 연결이 완료된 것은 아닙니다. 진단 로그는 stderr, JSON-RPC 메시지는 stdout에 출력합니다. Host에는 빌드된 JavaScript를 `node`로 직접 실행하는 명령을 설정합니다.

환경변수로도 설정할 수 있습니다.

```powershell
$env:DEVLINK_MCP_TRANSPORT = 'stdio'
$env:DEVLINK_MCP_LOG_LEVEL = 'debug'
node packages/mcp-server/dist/index.js
```

| 설정 | 인자 | 환경변수 | 규칙 |
| --- | --- | --- | --- |
| 전송 방식 | `--transport` | `DEVLINK_MCP_TRANSPORT` | 필수, 현재 `stdio`만 지원 |
| 로그 수준 | `--log-level` | `DEVLINK_MCP_LOG_LEVEL` | `debug`, `info`, `error`, 기본값 `info` |
| 브로커 설정 파일 | 없음 | `DEVLINK_BROKER_FILE` | CLI가 발행한 사용자 전용 JSON 파일의 절대 경로, 업무 요청 시 검사 |
| HTTP별 제한 | `--request-timeout-ms` | `DEVLINK_MCP_REQUEST_TIMEOUT_MS` | 1–300000 ms 정수, 소비자 기본 10000 |
| 호출 전체 제한 | `--call-timeout-ms` | `DEVLINK_MCP_CALL_TIMEOUT_MS` | 대기·세션·등록·bind 포함, 소비자 기본 30000 ms |

인자는 환경변수보다 우선합니다. 모르는 인자, 누락된 필수 설정, 지원하지 않는 값은 `CONFIG_INVALID`로 종료합니다. 로그에는 설정 필드명만 기록하고 입력값·토큰·임의 환경변수는 기록하지 않습니다. 브로커 파일을 찾거나 검증할 수 없으면 업무 도구가 `CREDENTIAL_FILE_INVALID`를 반환하며 진단은 계속 사용할 수 있습니다. 파일 형식과 권한은 HTTP 계약 문서를 따릅니다.

제한 시간 기본값은 현재 소비자 설정이며 운영 SLA 합의가 아닙니다. 전체 예산을 넘거나 취소해도 중앙 작업을 FAILED로 바꾸거나 재전송하지 않습니다. 알려진 taskId/requestKey를 보존하되 응답을 받지 못해 모르는 값을 만들지는 않습니다.

## MCP Host 설정 예시

다음은 `mcpServers` 형식을 사용하는 Host용 예시입니다. `command`와 첫 번째 인자는 각 컴퓨터의 실제 절대 경로로 변경합니다. 자동으로 사용자 설정 파일을 수정하지 않습니다.

```json
{
  "mcpServers": {
    "devlink": {
      "command": "C:\\Program Files\\nodejs\\node.exe",
      "args": [
        "C:\\Users\\SSAFY\\Documents\\DevLink\\packages\\mcp-server\\dist\\index.js",
        "--transport",
        "stdio"
      ],
      "env": {
        "DEVLINK_BROKER_FILE": "C:\\Users\\SSAFY\\.devlink-mcp\\claude-main\\broker.json"
      }
    }
  }
}
```

Codex는 같은 실행 명령을 다음 TOML 형태로 설정합니다. [공식 MCP 설정 문서](https://learn.chatgpt.com/docs/extend/mcp)의 `command`·`args` 형식을 따릅니다.

```toml
[mcp_servers.devlink]
command = 'C:\Program Files\nodejs\node.exe'
args = ['C:\Users\SSAFY\Documents\DevLink\packages\mcp-server\dist\index.js', '--transport', 'stdio']
[mcp_servers.devlink.env]
DEVLINK_BROKER_FILE = 'C:\Users\SSAFY\.devlink-mcp\codex-main\broker.json'
```

위 브로커 파일 경로는 예시이며 실제 CLI 발행 위치로 변경합니다. CLI·Rust 브로커가 아직 이 계약을 제공하지 않으면 업무 도구는 성공하지 않습니다.

Host마다 별도 프로필 파일을 지정합니다. `.devlink-runtime/runtime.json`의 `controlToken`은 CLI 제어용이며 MCP 자격 파일로 사용할 수 없습니다. 자격 파일은 프로세스마다 최초 업무 요청 시 한 번 읽습니다. 토큰 교체, 다른 팀·Agent로 전환할 때는 CLI에서 명시적으로 설정한 뒤 MCP 프로세스를 다시 시작합니다.

MCP SDK 2.4.0의 `serveStdio`가 기존 `initialize` 경로와 현대 프로토콜 경로를 처리합니다. `devlink_ping`은 입력 `{}`에 대해 다음 구조를 텍스트와 `structuredContent`로 반환합니다. 버전은 MCP 패키지 버전이며 브로커·중앙 서버의 준비 상태를 뜻하지 않습니다.

```json
{ "status": "ok", "version": "0.1.0" }
```

## 검증

```powershell
npm --prefix packages/mcp-server run check
npm --prefix packages/mcp-server test
```

테스트는 실제 Node 자식 프로세스와 공식 SDK Client를 사용합니다.

- legacy와 `2026-07-28` 프로토콜에서 도구 목록·호출 후 각각 두 번 실행합니다.
- 필수 설정 누락·잘못된 값은 종료 코드 1, stdout 0바이트와 구조화된 stderr 사유로 확인합니다.
- 30회 도구 호출 중 stdout의 모든 줄이 JSON-RPC인지, 로그와 테스트용 비밀값이 섞이지 않는지 검사합니다.
- stdin EOF 후 강제 종료 없이 코드 0으로 종료하는지, 초기화 전 EOF 후에도 재실행할 수 있는지 확인합니다.
- 명시적 close의 중복 호출과 stdin 리스너 정리를 확인합니다.
- 가짜 HTTP 브로커로 복구 경쟁, 세션 교체·만료, 8개 병렬 호출의 응답 상관관계, 제한 시간·취소·종료·재시도 상한을 검증합니다.
- 실제 OS 권한 검사에서 개인 설정 파일을 읽고 공개 권한 파일을 거부합니다. 현재 Windows DACL에서 실행했으며 POSIX 분기는 이 환경에서 실행하지 않았습니다.
- 두 프로토콜의 실제 stdio → 설정 파일 → 가짜 브로커 경로에서 7개 도구를 모두 호출합니다. 잘못된 입력과 actor 덮어쓰기는 세션 생성 전 거부합니다.
- 예제 모듈 추가, 공통 결과·오류 처리와 처리 중 EOF의 제한 시간 내 종료를 확인합니다.
- 프로필별 신원 분리, 동시 등록의 순차 처리, 재등록 이후 오래된 응답의 컨텍스트 변경 차단, 복구 시 신원 일관성 및 자격 교체 후 재시작을 검증합니다.
- 시스템 프록시가 설정된 자식 프로세스에서도 로컬 브로커 토큰을 프록시로 보내지 않는지 검사합니다. JSON Schema는 런타임 스키마에서 생성하며 `test`에서 파일의 최신 여부를 확인합니다.

테스트의 5초 제한은 테스트 실패를 감지하는 harness 값입니다. 브로커 요청·ACK·업무 처리의 운영 제한 시간이 아닙니다.

S15P31D108-64의 [실제 Host 호환성 실행기·검증 범위](docs/host-compatibility.md)는 Codex CLI와 Claude Code를 동일한 로컬 HTTP 테스트 브로커에 연결합니다. 오류·인증 컨텍스트·복구·자격 교체·종료를 실제 도구 이벤트와 대조하며, 비용이 발생하는 Host 실행은 `test:hosts`로 명시적으로 선택합니다. 실제 Rust의 세션·업무 API와 중앙 서비스 연동은 별도 확인이 필요합니다.

## 종료와 담당 경계

Host가 stdin을 닫으면 SDK가 서버·전송을 정리합니다. 직접 실행할 때 Ctrl+C로도 닫을 수 있습니다. `SIGINT`·`SIGTERM` 핸들러는 보조 경로이며 Windows 검증은 EOF와 명시적 close를 기준으로 합니다. 종료 시 `process.exit()`로 출력을 강제로 끊지 않습니다.

`src/runtime.ts`는 전송과 프로세스당 하나의 BrokerClient를, `src/server.ts`는 서버 팩터리와 도구 구성을 맡습니다. `src/tools/registry.ts`가 등록·검증·결과 처리를, `src/tools/business.ts`가 업무 스키마를, `src/broker/`가 HTTP와 세션을 담당합니다. 로거는 stderr만 사용하며 SDK 내부의 stdout 함수를 재정의하지 않습니다. 공유 브로커 실행·종료·중앙 연결·토큰 보관은 CLI 담당 범위로 유지합니다.

S15P31D108-50의 실제 브로커와 경로·인증·세션·ACK 계약 상호 검증, 업무 도구별 DTO 확인 및 S15P31D108-28 산출물 확인은 남아 있습니다. 이 모듈의 검증 결과를 해당 합의나 두 Agent의 전체 업무 왕복 검증으로 간주하지 않습니다. Jira 완료 판정은 미확인 수용 기준을 검증한 뒤 수행합니다.
