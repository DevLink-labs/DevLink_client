# 기여 가이드

DevLink의 버그 제보, 문서 개선과 기능 제안을 환영합니다. 이 문서는 기존 저장소의 개발·브랜치·커밋·PR 규칙을 옮긴 기본 가이드이며, 상세 기여 절차는 추후 보완합니다.

## 시작하기

- [커뮤니티 행동 규칙](CODE_OF_CONDUCT.md)을 확인해 주세요.
- 일반 오류와 기능 제안은 [Issues](https://github.com/SSAFY-URAYAHA/DevLink_client/issues)에 남겨주세요. 오류 제보에는 사용 환경, 재현 방법, 기대한 동작과 실제 동작을 포함합니다.
- 보안 취약점은 공개 Issue 대신 [보안 정책](SECURITY.md)의 제보 절차를 따릅니다.
- PR에는 변경 목적과 범위, 관련 Issue, 실행한 검사·테스트와 결과를 기록합니다. 비밀번호·토큰·개인 설정 파일은 포함하지 않습니다.

## 개발하기

소스에서 빌드하려면 Git, Rust stable 도구 체인, Node.js 24 이상이 필요합니다. Windows에서는 Visual Studio C++ Build Tools와 Windows SDK도 설치해야 합니다.

```sh
git clone https://github.com/SSAFY-URAYAHA/DevLink_client.git
cd DevLink_client

# MCP 서버 의존성 설치와 빌드
npm --prefix packages/mcp-server ci
npm --prefix packages/mcp-server run build

# CLI와 브로커 빌드
cd rust
cargo build --release --locked --workspace
```

CLI 실행 파일은 `rust/target/release/`에 생성됩니다. 소스 빌드 환경에서 연결할 때는 [설정 가이드](rust/docs/setup.md)의 `--mcp-entry`로 빌드한 MCP 진입점을 지정할 수 있습니다.

### 저장소 구조

```text
DevLink_client/
├── rust/
│   ├── crates/devlink-cli/      # CLI와 간편 설정
│   ├── crates/devlink-broker/   # 로컬 브로커와 중앙 서버 연동
│   └── docs/                   # 설치·설정·운영·개발 문서
├── packages/mcp-server/        # TypeScript MCP 서버
├── contracts/                 # 구성 요소 간 API·설정 계약
├── e2e/                       # 통합 시나리오 검증
└── .github/                   # CI와 릴리스 워크플로
```

### 검사와 테스트

저장소 루트에서 MCP 서버를 검사합니다.

```sh
npm --prefix packages/mcp-server run check
npm --prefix packages/mcp-server test
```

`rust/`에서 Rust 코드를 검사합니다.

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --all-targets
```

Linux·macOS의 `cargo test`에는 `-- --test-threads=1`을 추가합니다. 운영체제별 검증 방법은 [Rust 개발 문서](rust/README.md), 전체 서비스 통합 시나리오는 [E2E 문서](e2e/README.md)를 참고하세요.

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
