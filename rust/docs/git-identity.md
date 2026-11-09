# 본인 Git 이메일 관리 (SET-3-03)

사용자당 Git 이메일 한 개를 중앙에 등록하고, 브로커가 커밋 작성자를 본인 또는 미확인으로 판별한다. Git 상태 조회, 커밋 수집, 업로드 실행은 다른 담당자의 범위다. 이 구현은 수집기를 실행하거나 Git 설정을 변경하지 않는다.

## CLI

```powershell
devlink --config C:\work\profile\config.json git-identity register
devlink --config C:\work\profile\config.json git-identity register --email-stdin
devlink --config C:\work\profile\config.json git-identity status
devlink --config C:\work\profile\config.json git-identity delete
```

먼저 같은 프로필로 `auth login`을 수행한다. `register` 기본은 터미널 숨김 입력이다. `--email-stdin`은 stdin 한 줄을 읽고 LF/CRLF 줄 끝을 제거한다. 이메일은 명령 인자·환경 변수로 받지 않으며 출력하지 않는다. 등록·변경에는 같은 명령을 사용한다. 삭제는 이미 미등록이어도 성공한다. 이메일 원문이나 해시는 config·Agent 프로필에 저장하지 않는다.

성공 출력은 JSON 한 개다. 등록·조회는 `registered`와 `registeredAt`만 반환한다. 삭제·미등록 조회는 `{"registered":false,"registeredAt":null}`이다. 원문 재조회 기능은 서버가 제공하지 않는다. 다른 사용자의 이메일과 중복되면 `GIT_EMAIL_ALREADY_REGISTERED`를 출력하고 종료한다. 전송·응답 실패 후 자동 재시도하지 않으므로 `status`로 현재 상태를 먼저 확인한다. 서버 오류 본문과 잘못된 CLI 인자의 입력값을 오류 출력에 포함하지 않는다.

## 서버 계약

최신 `origin/develop`의 `d945f8b`에서 확인한 Git 신원 구현 `2853424`와 해시 구현 `1a09141`을 기준으로 한다. 사용자·Agent 인증은 서로 대체하지 않는다.

| 기능 | 경로 | 인증 |
| --- | --- | --- |
| 등록·교체 | `PUT /api/v1/auth/users/me/git-identity`, `{gitEmail}` | 사용자 세션 + CSRF |
| 등록 상태 | `GET /api/v1/auth/users/me/git-identity` | 사용자 세션 |
| 삭제 | `DELETE /api/v1/auth/users/me/git-identity`, 본문 없음 | 사용자 세션 + CSRF |
| 본인 대조값 | `GET /api/v1/agents/me/git-identity` | Agent Bearer |

등록 요청에만 이메일 원문을 사용한다. 브로커는 원문을 전송하는 별도 `/verify` API를 호출하지 않는다. 대조값의 `hashAlgorithm=SHA-256`, `normalization=TRIM_LOWERCASE`를 검증한다. Java `String.trim()`과 같이 앞뒤 U+0000–U+0020을 제거하고, 로케일에 의존하지 않는 소문자화 후 UTF-8 SHA-256의 소문자 64자리 hex를 사용한다. 비교 규칙이 다르거나 응답이 잘못되면 그 응답을 새 판별값으로 채택하지 않는다.

## 수집 담당자 연동

`devlink_broker::git_identity::IdentityCache`는 같은 브로커 인스턴스 내에서 복제해 공유하는 판별 핸들이다. `broker.rs`가 생성한 `git_identity`를 수집 태스크에 전달한다. 별도의 캐시를 매 커밋마다 생성하지 않는다.

```rust,ignore
let attribution = git_identity.attribute(commit_author_email).await;
// 이미 같은 규칙으로 해시한 값만 받는 수집기는 attribute_hash를 사용한다.
let attribution = git_identity.attribute_hash(commit_author_email_hash).await;
```

반환 타입 `AuthorAttribution`의 직렬화 결과는 `{"authorUserId":"51"}` 또는 `{"authorUserId":null}`이다. ID는 Java Long 범위를 손실 없이 표현하는 문자열이며, null은 미확인이다. 판별 함수는 이메일을 보관하거나 로그에 남기지 않는다. 수집 담당자는 이 결과의 ID만 실제 인덱싱·업로드 DTO에 반영하고 작성자 이메일 원문을 제외한다. 업로드 경로와 DTO 연결은 해당 수집 구현에서 수행한다.

브로커 시작 시 즉시, 이후 30초마다 `/agents/me`로 토큰 소유자의 userId를 확인하고 해시를 조회한다. 사용자 로그인 세션의 계정으로 대체하지 않는다. 두 요청이 완료되면 캐시를 한 번에 교체하며, 요청 대기 중에는 아직 만료되지 않은 기존 값을 사용한다. 전송 실패·5xx·잘못된 응답에서는 기존 값과 원래 만료 시각을 유지한다. 등록 해제 응답이나 두 API 중 어느 쪽의 확정 인증/권한 거부(401·403)를 받으면 즉시 비우고, 브로커 종료 시에도 비운다. 성공한 값도 마지막 성공 확인 후 30초가 지나면 미확인이므로, 느린 갱신 도중 만료되면 새 성공 응답 전까지는 미확인이다. 이메일 변경·삭제가 다른 프로세스에서 일어난 즉시 알리는 push API는 없으므로 주기 갱신과 만료를 따른다.

## 로컬 회귀

`rust` 디렉터리에서 실행한다. 아래 테스트는 임시 프로필·loopback 모의 서버를 사용하며 실제 중앙 서버나 Git 저장소를 변경하지 않는다.

```powershell
cargo test --offline --locked -p devlink-cli --test cli_git_identity -- --test-threads=1
cargo test --offline --locked -p devlink-broker --lib git_identity -- --test-threads=1
```

등록·교체·상태·반복 삭제, CSRF, 다른 사용자 중복 오류, 원문·해시 비노출, 잘못된 인자 비노출, 서버와 같은 해시 기준값, Agent 소유자 판별, 갱신 대기 중 기존 값 유지·새 값 교체, 일시 실패 시 원래 TTL 유지, 삭제·인증 거부·만료 후 미확인, 시작 갱신·종료 정리를 확인한다. 실제 서버·수집기·업로드 연동의 실행 확인과 구분한다.

2026-11-09 캐시 보완: 수정 전 응답을 지연한 회귀에서 갱신 중 기존 본인 ID가 null이 되는 것을 재현했다. 수정 후 캐시 단위 5건과 CLI·로컬 브로커 7건, 총 12건이 통과했다. workspace/all-targets Clippy `-D warnings`, rustfmt, 변경 공백 검사도 통과했다. 전체 workspace 테스트나 실제 중앙·수집기·업로드 검증을 새로 수행한 결과는 아니다. 로그는 `rust/target/git-identity-cache-before.log`, `git-identity-cache-unit.log`, `git-identity-cache-cli.log`, `git-identity-cache-clippy.log`에 남긴다.
