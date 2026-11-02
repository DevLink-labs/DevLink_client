# 로컬 브로커 신원 계약 v1

S15P31D108-62의 MCP 측 계약 산출물이다. Rust/중앙 서버가 현재 이 계약을 제공한다는 뜻은 아니다. 세션·업무 전체 HTTP 경로는 [기존 초안](../../../packages/mcp-server/docs/broker-contract-v0.1.md), 신원 갱신 규칙은 [인증 컨텍스트 문서](../../../packages/mcp-server/docs/agent-context.md)를 따른다.

| 파일 | 내용 |
| --- | --- |
| `credentials.schema.json` | 프로필별 MCP localToken 파일, CLI runtime.json과 구분 |
| `principal.schema.json` | 브로커가 확인한 user/team/선택적 Agent/toolType |
| `session.schema.json` | sessionId와 principal, HTTP data 내부 값 |
| `identity-fixtures.json` | 독립적으로 사용할 수 있는 정상·거부 wire 사례 |
| `envelope.schema.json` | 성공 data 또는 scope/outcome/식별자를 가진 오류 envelope |
| `context.schema.json` | -140 읽기 전용 현재 문맥. 신원은 live 중앙 검증, 프로젝트는 bound profile 출처 |
| `snapshot.schema.json` | -141 Rust GET /v1/connection의 ConnectionSnapshot·안내·안전한 issue |
| `connection.schema.json` | -141 MCP get_connection_status 결과 data. 확인된 MCP 실행 상태 추가 |
| `error-fixtures.json` | -63 오류 분류·세션 중단·결과 불명 계약 사례 |

JSON Schema draft 2020-12는 `packages/mcp-server/src/broker/schemas.ts`의 Zod 런타임 검증에서 생성한다. `npm --prefix packages/mcp-server run contracts:write`로 갱신하고 `contracts:check` 또는 `test`로 drift를 검사한다. fixture는 MCP 테스트에서 재사용하며 Rust 소비자도 같은 파일을 사용할 수 있다. 예제 토큰과 식별자는 테스트 전용 값이다.

`principal.toolType`은 v0.1 호환상 선택적이다. 런타임은 한 번 검증된 값의 복구 중 변경·소실을 거부한다. 자격 파일의 URL·권한·원자적 snapshot 검사, 같은 신원 유지, 소유권·중앙 인증은 JSON Schema 외의 구현 책임이다. Rust 소비자 테스트 및 실제 두 Host의 중앙 업무 왕복 검증은 아직 없다.

식별자 길이 상한은 Unicode code point 128개다. MCP의 refinement와 JSON Schema `maxLength`를 같은 단위로 유지하며 Rust는 바이트 길이 대신 `chars().count()`로 대조한다. whitespace/control은 언어별 `\s` 정의를 사용하지 않고 명시된 범위를 따른다.

세션 claim의 `agentId`는 확인 요청일 뿐 권한 근거가 아니다. Rust는 중앙에서 user/team의 Agent 소유를 확인해야 한다. 검증된/설정된 Agent의 recover는 동일 Agent로 bound되거나 명시적 오류여야 하며, 아직 bind 확인을 못 받은 pending Agent는 unbound 응답일 수 있다. 중앙 등록 멱등성과 requestKey는 후속 계약이며 이 소비자가 중복 등록을 방지한다고 보장하지 않는다.

2026-10-28: -63에서 [오류·시간 제한 소비자 계약](../../../packages/mcp-server/docs/error-contract.md)을 추가했다. envelope의 코드/scope와 key 필드는 MCP 측 제안이며 실제 Rust/중앙의 합의·소비자 테스트를 대신하지 않는다. 오류 object의 부가 message/details는 소비자에서 제거하며, JSON Schema는 input 기준으로 생성한다.

2026-11-02 (-140): GET /v1/context와 get_my_context({})는 세션 생성·bind·lease 갱신 없이 문맥을 조회한다. localToken 인증과 Origin 거부를 적용한다. 출처·미연결 상태·변경 차단·요청 헤더와 예제는 [온보딩 계약](../../../packages/mcp-server/docs/onboarding.md)을 따른다. context schema는 broker/onboarding.ts에서 생성하며 기존 v0.1 principal schema와 구분한다.

### MCP 프로세스 차단 진단 확장

MCP `get_connection_status`는 공통 ConnectionSnapshot에 `mcp: running`과 선택
`clientIssue`를 추가한다. `clientIssue`는 MCP 내부의 지속적인 terminal/contextBlocked
안전 차단이며, Rust provider의 현재 관측·`issue`와 구분한다. code/scope/outcome과
진단 호출의 requestId를 제공하고 provider가 이 필드를 공급하지는 않는다.

공통 setupReady의 환경 준비 공식과 원격 hostSession 관측은 유지한다. MCP 응답의
collaborationReady는 clientIssue가 있으면 항상 false이며, 없을 때는 공통 공식을
따른다. 따라서 환경 설정이 준비됐어도 현재 MCP 프로세스의 업무가 차단될 수 있다.
차단 중에는 register_agent 안내를 제거하고 원인 점검/복구 후 open_or_restart_host를
안내한다. 조회 자체로 차단 해제·세션/업무 변경이나 자격 재로딩을 수행하지 않는다.
