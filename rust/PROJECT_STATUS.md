# CLI·브로커 진행 현황

정리일: 2026-10-30 (Asia/Seoul). 현재 코드와 기존 검증 기록을 기준으로 정리했다. 이번 문서 정리는 새 기능 시험이나 외부 이슈의 완료 처리에 해당하지 않는다. 상세 계약은 [문서 안내](docs/README.md), 실행별 증거는 [검증 요약](docs/verification.md)을 따른다.

## 구현된 범위

| 영역 | 현재 코드 | 검증·남은 조건 |
| --- | --- | --- |
| CLI·설정·브로커 수명주기 | init/start/stop/status/config, 독립 프로세스·OS 잠금·복구 | Windows CLI 회귀 확인. 부팅 자동 실행·서비스 등록은 별도 |
| 사용자·팀·Agent 준비 | 로그인·세션, 팀 생성/가입/선택, Agent 생성/기존 ID 선택·토큰·프로젝트 바인딩 | 기능별 직접 HTTP/CLI·실제 중앙 검증 범위는 해당 문서 참고 |
| MCP 자격·권한 | broker.json 원자 발행, localToken 유지·명시적 회전, controlToken 분리, Agent ID 반영 | Host별 자동 설치·설정 병합 및 Unix 실행 검증은 별도 |
| 중앙 WebSocket·신원 | Bearer Upgrade, READY, /api/v1/agents/me 대조, generation·인증 무효화, heartbeat·재접속 | 고정 중앙과 실제 세션·presence 검증 기록 있음. 운영 배포 전체를 의미하지 않음 |
| 로컬 세션(-92) | 생성/bind/heartbeat/DELETE, fresh/recover 소유권, 90초 lease·presence | SDK→Rust와 고정 중앙·DB 검증 기록 있음. 실제 Host 업무 수용과 구분 |
| Agent 확인·목록·생성(-93) | 검증된 actor, Agent Bearer, requestKey 보관·재사용, 공식 생성 URL·REJECTED 응답 | 중앙 -67/-71 오프라인 정책과 -68 동시 생성의 최종 수용은 별도 |
| 상태·완료·목록·결과(-94) | 네 로컬 HTTP provider, version·파일·diff·결과 매핑, 오류·예산 | 직접 Rust→중앙·DB 검증. MCP 최종 DTO(-97)·구조화 verification 계약은 별도 |
| 수신함·ACK(-95) | 안전한 영속화, event/task 중복 방지, 저장 후 ACK, CLI inbox list/clear | 최신 보완에서 전체 Rust 168건 통과·2건 제외. 실제 중앙 17건 ACK 및 기록 실패 무ACK, Gemini 신규 결함 0건 |

## 다음 확인 범위

1. -96 상태·결과 알림 수신과 재접속 후 조회 대조.
2. -97 MCP 입력/출력 DTO를 Rust·중앙 계약에 수렴. 현재 MCP의 version 없는 변경 요청과 구조화 result 입력은 그대로 사용할 수 없다.
3. -67/-68/-71 중앙 정책·전달 이력, -92 등 선행 이슈의 최종 수용을 담당 범위별 확인.
4. -52 전체 통합, -66 실제 Codex/Claude Host에서 두 사용자의 A→B/B→A 요청·수락·수정·검증·결과 조회.
5. Ubuntu 실행 및 Windows/Ubuntu CI, 설치·Host별 프로필 운영 정책.

## 검증 기록의 기준

최근 -95 시험은 client `0516473` 이후 작업 트리(검토 HEAD `3ee8fc7`)와 중앙 `0e8c2b7`, Windows MSVC, Java 21, 전용 PostgreSQL 17.11을 사용했다. Flyway V1~V6·Hibernate validate를 유지했고, 작업 18건은 모두 PENDING이었다. 검토 결과는 사용자 제공 Gemini 보고이며 직접 실행 로그와 구분한다.

이전 -93/-94·세션 실행은 중앙 커밋·DB 설정·검증 범위가 다르다. [검증 요약](docs/verification.md)과 기능별 문서에서 별도로 읽는다. 세션·ACK·목록 조회가 사용자 수락이나 자동 편집을 의미하지 않는다.
