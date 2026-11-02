# 요청자 상태·결과 알림 (S15P31D108-96)

중앙 `TASK_STATUS_CHANGED`와 `TASK_COMPLETED`는 인증된 **요청자 Agent**에게 전달된다. Rust는 READY 이후 현재 연결 세대를 확인해 typed DTO를 파싱하고, 현재 중앙 origin/요청자 Agent별로 private `notifications.json`에 원자 저장한다. 신규 요청을 받는 `TASK_CREATED` 수신함·수신자 검증·저장 후 ACK와 구분하며 알림에는 ACK를 전송하지 않는다.

```powershell
devlink --config C:\profiles\codex.json notifications --limit 20
devlink --config C:\profiles\codex.json status
```

현재 프로필에 바인딩된 Agent·중앙 origin의 로컬 알림만 표시한다. taskId/version은 JSON 문자열로 반환해 Long 정밀도를 보존한다. 동일 eventId 재접수는 중복 저장하지 않고, 같은 ID나 task/version의 충돌은 거부한다. task별 최고 version을 표시하므로 늦게 도착한 이전 상태가 완료/실패 상태를 덮지 않는다. 보관 데이터는 프로세스 재시작 후에도 유지된다. 토큰이나 중앙 권한을 이 파일에서 만들지 않는다.

알림은 참고용이며 중앙 상태의 원본이 아니다. 상태 이벤트에 실패·거절 사유나 변경파일이 없으므로 `detailRequired=true`와 다음 행동을 표시한다. **같은 프로필의 소유한 bound MCP 세션에서 기존 `get_task_result(taskId)` 및 `get_tasks(direction="sent")`를 호출**해 -94의 인증된 로컬 `/v1/tasks/result`·`/v1/tasks/list` 중계를 통해 중앙 상세 결과를 대조한다. 알림 유실·단절 중 누락도 같은 목록/상세 조회로 확인한다. CLI가 Host 세션을 빼앗거나 새 MCP 도구/push를 추가하지 않는다.

저장 작업은 별도 blocking worker와 bounded queue(16개)를 사용해 WebSocket heartbeat/control을 막지 않는다. 접수한 쓰기는 소켓 세션이 종료되어도 공유된 16개 permit 아래에서 계속 처리한다. 같은 프로필의 쓰기는 async mutex로 직렬화하며, 이전 프로세스와의 OS 파일 try-lock 경합은 즉시 거부·계수하고 기존 기록을 유지한다. malformed·conflicting·queue full·디스크 오류는 health/status의 `notificationRejections`에 반영하고 기존 중앙 업무·연결·상세 조회는 유지한다. 오류로 작업을 FAILED로 만들거나 자동 수락·업무 재실행·중앙 재전달하지 않는다. 이전 세대 소켓의 새 메시지는 기록하지 않는다. 이미 현재 신원으로 접수된 기록은 그 origin/Agent 범위에만 보관한다.

보관 상한은 1024 이벤트·16MiB다. 상한에 도달하면 기존 최고 version을 지우며 새 상태를 낮추지 않고 새 기록을 거부하고 중앙 조회를 안내한다. 운영자가 로컬 기록만 초기화하려면 브로커를 중지한 뒤 `devlink --config <file> notifications --clear`를 실행한다. 실행 중 clear는 거부하며 중앙 작업·결과는 변경하지 않는다. 손상된 기록은 자동 덮어쓰기하지 않고 CLI에 `NOTIFICATION_STORE_UNAVAILABLE`과 중앙 조회 안내를 표시한다.

## 검증

- 단위: 실제 private 저장/권한·재열기·origin/Agent 격리·중복·역전·버전/ID 충돌·Long 정밀도·손상 보존·상한·프로세스 lock·명시적 초기화.
- 실제 loopback WebSocket: 요청자 이벤트 수신, ACK 없음, 완료 후 낮은 version, 실패한 기록에도 연결 유지, 교체 세대의 이전 소켓 거부.
- 실제 중앙/DB: `npm.cmd --prefix packages/mcp-server run build`, Rust native build 후 격리 loopback 중앙과 `DEVLINK_TEST_CENTRAL_URL`, `DEVLINK_TEST_CENTRAL_COMMIT`, 절대 `DEVLINK_TEST_CLI`를 설정하고 `node packages/mcp-server/scripts/verify-notifications.mjs` 실행. 실제 상태 변경/완료 송신 → Rust → CLI 표시 → 기존 SDK/로컬 권한 조회 결과를 대조한다. 이 기능별 검증은 실제 LLM Host 코드 수정 수용(-66)과 별도다.

공용 private_file 데이터 경로 검사는 호출자별 크기 상한을 적용한다. 자격 전용 16KiB 검사를 데이터에도 적용하던 문제를 상한 시험으로 재현·수정했으며 MCP 자격 파일 자체의 16KiB 제한·권한·경로 검사는 유지한다.
