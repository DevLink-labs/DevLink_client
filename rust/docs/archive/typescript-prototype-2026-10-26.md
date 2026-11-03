# 2026-10-26 TypeScript 실습 코드 분석 이력

이 문서는 당시 코드·환경·검토 상태를 보존한 과거 기록이다. 미구현·차단·검토 대기 표현은 현재 상태가 아니다. 현재 계약과 결과는 [문서 안내](../README.md)·[검증 요약](../verification.md)을 따른다.

분석일: 2026-10-26. 사용자가 제공한 로컬 `C:/Users/SSAFY/cli-pratice`의 소스를 읽었으며 원본은 변경하지 않았습니다. 현재 Devlink 저장소의 초기 커밋에는 README만 있어 포팅 대상 코드 자체는 커밋에 포함되어 있지 않습니다.

| 기존 파일 | 확인한 동작 | Rust 반영 |
| --- | --- | --- |
| `src/index.ts` | commander init/config/start/status/stop, detached spawn, Health polling | clap, 같은 기본 명령, 준비 확인, 독립 실행 |
| `src/config.ts` | `$HOME/.devlink-practice/config.json`, 3필드, 생성 시 wx | camelCase 3필드 호환, 새 `.devlink` 경로, 타입/범위 검증, 설정 수정·원자 교체 |
| `src/broker.ts` | 127.0.0.1:43127, GET /health, POST /shutdown, SIGINT/SIGTERM 정리 | 같은 기본 포트·경로, 인증과 인스턴스 식별 추가, OS 잠금, 정리 시간 제한 |
| `src/server-client.ts` | DEVLINK_TOKEN, auth/auth_ok/auth_error, 5초 인증 제한, 2초 재연결, 4001 중단 | 같은 wire protocol, 백오프·TLS·Heartbeat·취소 가능 대기 |
| `src/mock-server.ts` | 개발용 고정 토큰, 인증 후 echo, 잘못된 JSON 시 종료 | 환경 변수로 토큰을 받는 Rust 예제, 테스트용 Mock 별도 구성 |
| `src/agents/codex.ts` | `codex mcp add`로 외부 OpenAI 문서 MCP URL 등록 | 실제 DevLink MCP 구현이 아니므로 자동 등록 포팅 제외. 담당자와 등록 방식 협의 필요 |

기존 구현은 Health 응답만으로 중복 실행을 판단하고 모든 Health 오류를 중지로 표시합니다. Rust는 OS 잠금과 인증된 인스턴스 ID/PID 확인을 함께 사용합니다. 로컬 shutdown도 인증을 요구합니다. 기존 TypeScript 중앙 Mock 서버에는 Rust 브로커의 `DEVLINK_TOKEN`을 해당 서버의 개발용 토큰과 동일하게 설정하면 연결할 수 있습니다.

중앙 메시지의 현재 계약은 `auth { token }` → `auth_ok` 또는 `auth_error`, 인증 성공 후 `hello { message }`, 서버 `echo { message }`입니다. 실제 작업 요청이나 응답 라우팅 계약은 기존 실습 코드에도 없으므로 구현 완료로 간주하지 않습니다.
