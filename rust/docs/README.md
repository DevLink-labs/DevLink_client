# Rust CLI·브로커 문서 안내

2026-10-30 (Asia/Seoul). 현재 구현·사용법은 기능별 문서를 기준으로 읽는다. 검증 결과는 실행한 커밋과 환경에 한정되며, 이전 실행 수를 합산하거나 전체 MVP 완료로 확대하지 않는다.

## 시작과 현재 상태

- [CLI 사용법·빌드](../README.md)
- [프로젝트 진행 현황](../PROJECT_STATUS.md)
- [검증 요약과 재현 명령](verification.md)

## 기능별 계약

| 영역 | 기준 문서 |
| --- | --- |
| 간편설정 입력·출력·단계·오류·협업 경계 (후속 구현 규격) | [setup v1](../../contracts/setup/v1/README.md) |
| setup 실행 흐름·설정 자동 초기화·로그인·팀 선택 (작업 02·03·04·05) | [setup.md](setup.md) |
| 사용자 로그인·세션 | [user-auth.md](user-auth.md) |
| 팀 생성·가입·선택 | [team-setup.md](team-setup.md) |
| Agent 생성·선택·토큰·로컬 바인딩 | [agent-bootstrap.md](agent-bootstrap.md) |
| MCP 프로세스·자격 파일·로컬 HTTP | [mcp-interface.md](mcp-interface.md) |
| 중앙 WebSocket·READY·presence·heartbeat | [central-websocket-contract.md](central-websocket-contract.md) |
| 중앙 신원 조회·재접속·인증 무효화 | [ready-identity.md](ready-identity.md) |
| 로컬 세션·소유권·lease | [local-sessions.md](local-sessions.md) |
| Agent 확인·목록·작업 생성 | [business-relay.md](business-relay.md) |
| 작업 상태·완료·목록·결과 | [task-operations.md](task-operations.md) |
| 수신함·중복 방지·저장 후 ACK·CLI 조회 | [task-inbox.md](task-inbox.md) |
| 요청자 상태·결과 알림·보관·조회 대조 | [task-notifications.md](task-notifications.md) |
| 수신 작업·요청자 알림의 로컬 업데이트 조회 (`get_updates`) | [local-updates.md](local-updates.md) |
| Host 훅용 로컬 업데이트 요약 (`devlink updates-hook`) | [update-hook.md](update-hook.md) |

MCP 도구의 소비자 계약·Host 설정은 [MCP 문서](../../packages/mcp-server/README.md), 중앙과의 계약 차이는 [중앙 연동 결정표](../../contracts/central/README.md)를 참고한다. Rust provider 구현과 MCP의 최종 입력 DTO 수렴(-97), 실제 Host 전체 왕복(-66)은 별도 범위다.

## 과거 검증 이력

아래 문서의 미구현·검토 대기·차단 상태는 해당 실행 시점의 기록이다.

- [초기 빌드·MCP·세션·WebSocket 검증](archive/verification-2026-10-27-30.md)
- [세션 신원·소유권·실제 중앙 재검증](archive/session-verification-2026-10-29-30.md)
- [초기 TypeScript 실습 코드 분석](archive/typescript-prototype-2026-10-26.md)

완료된 일회성 검토 요청서와 작업별 중복 보고서는 기능 문서에 핵심 결과를 통합한다. raw 로그·검토 bundle·manifest는 ignored `rust/target/` 또는 해당 실행 기록에 명시된 private 경로의 보조 자료이며 제품 사용 문서가 아니다.
