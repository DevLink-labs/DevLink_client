# 인증 컨텍스트와 프로필 자격 파일 (S15P31D108-62)

작성일: 2026-10-27 (Asia/Seoul). MCP 측 구현 및 공유 계약이다. 실제 Rust 세션·업무 API와 중앙 인증의 종단 간 검증은 남아 있다.

## 신원의 출처

MCP는 `DEVLINK_BROKER_FILE`의 절대 경로에 있는 프로필 전용 자격 파일을 최초 업무 요청 시 한 번 읽는다. CLI가 사용자 전용 디렉터리에 원자적으로 발행하는 파일을 전제로 하며 MCP가 자격 파일을 생성하거나 갱신하지 않는다. 프로필 이름과 경로를 자동 탐색하지 않는다.

```json
{
  "schemaVersion": 1,
  "brokerUrl": "http://127.0.0.1:43127",
  "localToken": "example-only-not-a-real-token",
  "agentId": "agent-codex-main"
}
```

`agentId`는 등록 전에는 생략한다. `userId`, `teamId`, `toolType`은 파일이나 Tool 입력에서 추론하지 않는다. 인증된 브로커 세션 응답의 `principal`만 사용한다. `toolType`은 브로커가 확인한 `codex` 또는 `claude_code`다. 기존 v0.1 호환을 위해 생략할 수 있지만 한 번 확인된 값이 복구 응답에서 사라지거나 바뀌면 거부한다. 명시적 재등록의 bind 응답에서는 같은 Agent의 확인된 도구 유형을 갱신할 수 있다.

검사 항목은 버전·알 수 없는 필드·필수 값·16 KiB 상한·일반 파일·직접 상위 디렉터리 링크 여부·파일 권한이다. 파일을 연 뒤 inode/device와 크기/수정 시각을 비교하고 제한된 버퍼로 읽는다. POSIX는 사용자 소유 0600 상당 파일 및 다른 사용자 쓰기가 차단된 부모, Windows는 파일 소유자와 사용자/SYSTEM/Administrators 외 읽기 권한·부모 쓰기 권한을 검사한다. 같은 OS 사용자나 관리자 프로세스에 대한 격리를 제공하지 않는다. Windows DACL 분기는 실제 실행했고 POSIX 분기는 이 환경에서 실행하지 않았다.

주소는 명시적 포트가 있는 `http://127.0.0.1:<port>`만 허용한다. DNS 이름·IPv6·축약 IP·사용자 정보·경로·쿼리·프래그먼트·redirect를 거부한다. 전용 `node:http` Agent의 `proxyEnv: {}`로 시스템 프록시와 전역 fetch dispatcher를 사용하지 않는다. Bearer 토큰은 HTTP 헤더에만 넣고 로그에 기록하지 않는다.

`runtime.json`의 `controlToken`은 CLI 관리 자격이다. 이 파일을 읽어 MCP 토큰으로 변환하지 않는다. 현재 Rust CLI는 위 프로필 파일을 발행하지 않는다.

## 등록과 갱신

1. 최초 업무 요청에서 `POST /v1/sessions`로 브로커가 확인한 user/team 및 선택적 Agent를 받는다.
2. unbound 세션은 `register_agent`만 호출할 수 있다. 나머지는 `AGENT_UNREGISTERED`로 거부한다.
3. 중앙 등록 결과의 Agent ID를 확인한 뒤 같은 로컬 세션에 bind를 요청한다. user/team/Agent/session 일관성을 검증한 응답만 불변 snapshot으로 교체한다.
4. 동시 `register_agent`는 프로세스 안에서 순차 처리한다. 등록이 진행 중일 때 들어온 업무 호출은 대기하고 최종 bind snapshot을 사용한다. 이미 전송된 업무 요청은 자동 재실행하지 않는다.

중앙 등록이 성공하고 bind가 실패하면 발행받은 Agent ID를 pending 상태로 복구용으로 기억하고 검증되지 않은 principal은 게시하지 않는다. 중앙 등록 응답 전에 lease가 없어져도 해당 ID를 보존한다. 다음 호출은 recover로 실제 바인딩을 다시 확인한다. 브로커에 살아 있는 unbound lease가 남았다면 업무 호출은 `AGENT_UNREGISTERED`이며, 사용자가 다시 `register_agent`를 호출해 bind를 완료할 수 있다. 자동 등록·업무 재전송은 하지 않는다. 재등록 결과가 다른 Agent ID이면 중단한다. 다른 팀·Agent로 전환하려면 CLI에서 명시적 설정 후 MCP를 재시작한다.

이 재등록 경로는 중앙이 동일 Agent를 반환할 때만 성공한다. 중앙이 중복 Agent를 발급하면 MCP는 신원 변경을 거부하지만 중앙의 중복 생성 자체를 되돌리지는 못한다. 등록 응답 자체가 취소·timeout으로 유실되면 ID를 알 수 없고, 프로세스 재시작 시 메모리의 pending ID도 사라진다. 이때의 중복 방지는 중앙의 requestKey·등록 멱등성 계약(-63 및 중앙 구현)이 필요하다. MCP가 해결했다고 주장하지 않는다. bind 실패 후에는 제한된 세션 복구를 예약하되 등록·업무는 자동 재전송하지 않는다.

각 요청은 당시의 불변 세션 snapshot을 사용한다. 오래된 heartbeat나 업무 오류는 새 bind 결과를 무효화할 수 없다. bind 중 lease가 없어지거나 종료되면 늦게 도착한 응답을 게시하지 않는다. 복구 응답이 기존 user/team/Agent와 다르면 `BROKER_CONTEXT_INVALID`로 중단한다.

-142부터 모든 자동 MCP claim은 첫 시도부터 recover다. 재시작 후 먼저 복구한 인스턴스가 소유하며 다른 활성 소유자를 빼앗지 않는다. fresh 교체는 Rust의 명시적 API에만 남긴다. 업무 요청은 재시도하지 않고 세션 복구만 제한적으로 재시도한다.

Rust는 `/v1/sessions`의 `agentId`를 신원 증거로 취급하면 안 된다. 중앙에서 해당 Agent의 user/team 소유를 확인한 뒤 bound principal을 발행해야 한다. configured/검증된 Agent ID가 있는 recover는 같은 Agent에 bound인 응답 또는 명시적 오류를 반환해야 한다. pending ID만 있는 recover는 아직 unbound일 수 있다. 이 재검증은 테스트 대역의 편의 동작으로 대체되지 않는다.

## 입력과 진단

Tool 등록 계층과 BrokerClient 양쪽에서 최상위 `userId`, `teamId`, `agentId`, `sessionId`, `principal`, `context` 덮어쓰기를 거부한다. 입력과 호출 옵션을 접수 시 복사해 대기 중 호출자가 내용을 바꿔도 적용되지 않게 한다. 업무 대상인 `recipientAgentId` 등의 DTO는 기존 도구 계약을 따른다. 중앙 서버도 세션 신원을 재검증해야 한다.

debug 로그는 서버가 생성한 UUID requestId, 허용된 operation, 고정된 오류 코드로 호출 시작/성공/실패를 연결한다. 프로필 토큰·user/team/Agent·업무 본문·임의 오류 문자열은 기록하지 않는다. 기본 info 설정에서는 호출별 debug 이벤트를 출력하지 않는다.

## 검증과 남은 수용 기준

`npm --prefix packages/mcp-server run check`와 `npm --prefix packages/mcp-server test`로 타입·계약 drift·실제 stdio와 SDK·가짜 브로커를 검증한다. 새 검증은 malformed principal, bind 신원 변경, 동시 등록, 오래된 응답, lease 소실, 두 Host 프로필 분리, 토큰 교체 후 재시작, 잘못된 파일/권한/링크, 프록시 우회와 비밀 비노출을 포함한다.

이 절을 작성한 2026-10-28에는 Rust의 `/v1/sessions`, bind/heartbeat/delete, 업무 route, 프로필별 localToken 발행·검증 및 중앙의 user/team/Agent 연결이 구현 전이었다. 2026-11-09 갱신: 이후 Rust 세션 API(-92)·업무 중계와 중앙 연결이 구현됐다. 실제 Rust·중앙·Host에서 무엇을 확인했는지는 [중앙 왕복 검증](central-roundtrip.md)과 [두 Host 온보딩 검증](onboarding-host-verification.md)을 따른다. 로컬 테스트 통과나 스토리 커밋만으로 Jira Done 처리하지 않는다.

2026-10-28 갱신: -63의 [공통 오류·시간 제한 계약](error-contract.md)에서 업무 403과 명시적 local_auth 거부를 구분했다. 업무 BROKER_FORBIDDEN은 세션을 영구 중단하지 않고 로컬 LOCAL_AUTH_FORBIDDEN은 중단한다. requestKey·원본 요청 ID 보존과 전체 시간 예산을 추가했으며 실제 Rust·중앙 계약 검증은 남아 있다.
