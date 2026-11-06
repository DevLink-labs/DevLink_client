# DevLink

## 간편설정 규격

[간편설정 호출·단계·오류 규격 v1](contracts/setup/v1/README.md)은 대화형/비대화형 입력, 진행·결과·오류 응답, 복구·종료 코드와 등록 API·MCP 설정·연결 상태의 연동 경계를 정의합니다. `setup`은 설정 준비·로그인·팀/프로젝트/도구 선택과 Agent 생성·재사용까지 연결되어 있습니다. 새 Agent의 토큰 준비와 MCP 설정 통합은 후속 단계이며, 현재 동작과 복구 방법은 [간편설정 구현 문서](rust/docs/setup.md)를 따릅니다.

## MCP 서버

S15P31D108-59 실행 기반, -60 브로커 호출·세션 복구, -61 공통 도구 등록과 7개 업무 도구의 실행 방법 및 검증 절차는 [MCP 서버 문서](packages/mcp-server/README.md)를 참고합니다. 로컬 HTTP·도구 
JSON은 MCP 측 v0.1 초안이며 실제 Rust 브로커와의 상호 검증은 남아 있습니다.

사용자는 [Agent 요청·수락·결과 등록 지침](packages/mcp-server/docs/agent-workflow.md)에서 수신자 지정부터 사용자 수락, Host의 로컬 수정·검증, 완료·실패 조회까지 확인할 수 있습니다.

실제 중앙 연동의 인증·Agent bootstrap·DTO 차이와 후속 구현 조건은 [중앙 연동 결정표](contracts/central/README.md)에 정리했습니다. 확인한 develop SHA와 미제공 API를 구분하며, 기존 로컬 계약의 실제 연동 완료를 뜻하지 않습니다.

## Git Flow

[Notion Git Flow 전략](https://www.notion.so/fdd35a4f3ca6837fb6ef8101afa7fb68)을 기준으로 브랜치를 관리합니다.
Notion 문서에 혼용된 `main`과 `master`는 이 저장소의 운영 브랜치인 `master`로 통일합니다.

### 브랜치 역할

| 브랜치      | 역할                       |
| ----------- | -------------------------- |
| `master`    | 배포 가능한 운영 버전      |
| `develop`   | 다음 배포를 위한 개발 통합 |
| `feature/*` | 기능 개발                  |
| `release/*` | 배포 준비 및 최종 수정     |
| `hotfix/*`  | 운영 환경 긴급 수정        |

- `master`, `develop`에는 직접 푸시하지 않고 Pull Request(PR)를 통해 반영합니다.
- `feature/*`는 `develop`에서 분기하고, 하나의 기능이 완성되면 `develop`으로 PR을 생성합니다.
- `release/*`는 `develop`에 배포할 기능이 모이면 생성합니다. 배포 준비 중 발생한 버그는 해당 브랜치에서 수정하고 `develop`에도 반영합니다. 배포 가능한 상태가 되면 `master`와 `develop` 양쪽으로 PR을 생성합니다.
- `hotfix/*`는 `master`에서 분기해 긴급 버그를 수정합니다. 수정이 완료되면 `master`와 `develop` 양쪽으로 PR을 생성합니다.

### 브랜치 명명 규칙

```text
{type}/{scope}/{description}
```

- `type`: `feature`, `release`, `hotfix`.
- `scope`: 아래 작업 영역 표를 사용합니다.
- 배포 브랜치는 `release/{version}` 형식을 사용합니다.

| Scope     | 대상                       |
| --------- | -------------------------- |
| `front`   | Frontend                   |
| `back`    | Backend                    |
| `ai`      | AI / ML                    |
| `data`    | 데이터 수집 / 전처리 / ETL |
| `bigdata` | 대용량 데이터 처리         |
| `spark`   | Spark / 분산 처리          |
| `kafka`   | Kafka / 분산 처리          |
| `infra`   | Docker / 서버 / 배포 환경  |
| `common`  | 공통 작업                  |

```text
feature/front/login
feature/back/auth-api
feature/ai/recommendation
release/1.0.0
hotfix/back/auth-error
```

### 커밋 규칙

```text
type(scope): message
```

커밋의 `scope`는 브랜치 명명 규칙의 작업 영역과 동일하게 사용합니다.

| Type       | 설명                              |
| ---------- | --------------------------------- |
| `feat`     | 기능 추가                         |
| `fix`      | 버그 수정                         |
| `refactor` | 코드 구조 개선                    |
| `perf`     | 성능 개선                         |
| `docs`     | 문서 수정                         |
| `test`     | 테스트 추가 / 수정                |
| `style`    | 동작에 영향을 주지 않는 코드 수정 |
| `chore`    | 설정 / 패키지 / 빌드 / 기타 작업  |
| `data`     | 데이터셋 / 전처리 변경            |

### Pull Request 규칙

PR은 하나의 기능 또는 작업 단위로 생성하고, 제목은 커밋과 동일한 `type(scope): message` 형식을 사용합니다.

```text
feature/front/login → develop
feature/ai/recommendation → develop

release/1.0.0 → master
release/1.0.0 → develop

hotfix/back/auth-error → master
hotfix/back/auth-error → develop
```

PR 제목 예시:

```text
feat(front): 로그인 페이지 구현
feat(ai): 추천 모델 구현
perf(bigdata): 데이터 처리 성능 개선
```
