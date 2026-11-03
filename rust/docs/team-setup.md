# CLI 팀 설정

S15P31D108-90. 먼저 [사용자 로그인](user-auth.md)을 완료한다. 모든 권한은 사용자 세션과 중앙의 현재 멤버십으로 검증한다.

간편설정의 조회·선택·재사용은 [setup 작업 05](setup.md#작업-05-팀-조회선택과-재사용)를 참고한다. 기존 개별 명령의 명시적 선택 동작과 달리 setup은 기존 Agent 바인딩과 충돌하는 팀 선택을 저장 전에 중단한다.

```powershell
devlink team create --name "My team"
devlink team list
devlink team join
devlink team select --id 1
devlink team current
devlink team members --id 1
```

team join은 초대코드를 숨김 입력한다. 자동화에서는 표준입력 한 줄과 --code-stdin을 사용하며 코드 인수/환경변수는 받지 않는다. create 응답의 팀ID·OWNER·초대코드는 생성 사용자가 확인할 수 있고, join 응답은 신규/기존 참여(replayed)를 구분한다. 초대코드는 일반 목록이나 선택 팀 파일에 저장하지 않는다.

생성·참여는 선택 팀을 자동 변경하지 않는다. team select는 상세와 멤버 목록에서 현재 검증된 사용자의 소속·역할을 대조한 뒤 .devlink-runtime/selected-team.json에 origin/userId/teamId만 사용자 전용 권한·원자 교체로 저장한다. team current와 후속 Agent bootstrap도 현재 사용자·팀 멤버십을 다시 확인한다. 수동 userId나 파일에 쓴 다른 사용자의 선택을 권한 근거로 쓰지 않는다.

팀 선택은 Agent/MCP 자격과 사용자 세션을 묵시적으로 바꾸지 않는다. 선택 응답의 agentBindingChanged는 false이며 Agent 전환·토큰 발급·브로커 재시작은 후속 bootstrap의 명시적인 작업이다. 금지된 팀·확인되지 않은 멤버십은 기존 선택과 다른 자격을 보존한 채 실패한다. 만료·로그아웃된 세션은 팀 명령을 실행할 수 없다.

create는 중앙에 한 번만 전송한다. 생성 후 멤버십 확인에 실패하면 알려진 팀ID와 함께 목록 확인을 안내한다. 중앙이 저장했을 수 있는 통신 실패를 자동 재시도해 팀을 중복 생성하지 않는다. join의 중복 참여는 중앙의 원자적/idempotent 처리를 사용하며 멤버의 역할을 임의로 바꾸지 않는다.

검증은 cli_teams의 실제 CLI 계약 fixture와 scripts/verify-team-setup.mjs의 두 사용자 실제 중앙 API로 나눈다. 재현 스크립트는 격리 loopback 중앙만 허용하고 합성 계정 두 개를 생성해 OWNER 자동 가입·초대/중복 참여·다른 팀 접근 거부·선택 사용자 바인딩·로그아웃 차단을 검사한다. DEVLINK_TEST_CENTRAL_URL과 절대 DEVLINK_TEST_CLI를 지정해 node scripts/verify-team-setup.mjs로 실행한다. 실제 결과와 Opus 검토는 날짜별 Notion 및 PR에 기록하며 전체 MCP 업무 왕복 완료를 뜻하지 않는다.

2026-10-29 검증: Windows MSVC에서 팀 CLI fixture5건과 인증 회귀8건이 통과했다. 실제 중앙 a4858d6 + 격리 PostgreSQL17.11의 서로 다른 두 계정으로 생성자 OWNER 가입·명시적 선택·다른 팀 접근 거부 시 선택 보존·잘못된 초대코드·중복 참여·사용자 바인딩·로그아웃 후 명령 차단을 확인해 재현 스크립트 종료0을 얻었다. 합성 비밀번호/사용자 쿠키의 출력 비포함도 확인했다. 최종 전체 Rust 회귀와 Opus 결과는 PR·Notion에 기록한다.
