# Git 수집 범위 제한 (IDX-3-03)

2026-11-10 (Asia/Seoul), S15P31D108-187. 브로커가 중앙에 올리는 Git 근거를 허용 필드로만 구성한다. 커밋 이력 동기화(IDX-3-01, S15P31D108-185)와 작업 파일 수집(IDX-3-02, S15P31D108-186)은 업로드 본문을 `devlink_broker::git_scope`의 타입으로 만든다. 이 모듈은 Git을 실행하거나 중앙에 요청하지 않는다.

## 허용·제외

| 구분 | 항목 |
| --- | --- |
| 허용 | 원격 해시(`repoKey`), 브랜치, 파일 상대 경로, 변경 유형, 커밋 SHA와 시각, 작성자(본인 userId 또는 미확인) |
| 제외 | 절대 경로, diff·파일 내용, 원격 URL 원문, 커밋 작성자 이메일 원문 |

타입에는 제외 항목을 담을 필드가 없다. 생성자는 아래 검증을 통과한 값만 받는다.

- `RepoKey`: 정규화한 원격 URL의 SHA-256(소문자 16진수 64자리)만 받는다. 값은 `dependency_repo_key`로 계산한다. URL 원문은 거부한다.
- `RelativePath`: 빈 값, `/` 시작, `\`, 드라이브 문자(`C:`), 빈·`.`·`..` 경로 조각, 제어 문자를 거부한다. 커밋 경로는 UTF-16 500단위(Java `String.length`) 이하다. `from_absolute(root, path)`는 저장소 루트 아래 절대 경로만 상대 경로로 바꾸고, 루트 자신·루트 밖·`..`이 섞인 경로는 거부한다.
- `Author`: SET-3-03의 `AuthorAttribution` 결과만 받는다. 이메일은 받지 않는다. 본인 userId(양의 Java long)가 아니면 미확인(`null`)이다.
- 커밋 SHA는 소문자 16진수 40·64자리, 커밋 시각은 `yyyy-MM-ddTHH:mm:ss[.f{1,6}](Z|±hh:mm)`이다. Git이 기록한 오프셋의 절댓값이 `18:00`을 넘으면 같은 시각을 UTC로 변환해 보낸다.

## 커밋 배치 (`commits:batch`)

`POST /api/v1/teams/{teamId}/git/commits:batch`(Agent 토큰) 본문이다. 중앙은 정의되지 않은 필드를 거부한다.

```json
{
  "repoKey": "<64 hex>",
  "commits": [
    {
      "commitSha": "<40|64 hex>",
      "committedAt": "2026-11-10T12:34:56+09:00",
      "authorUserId": null,
      "files": [{"relativePath": "src/login.ts", "changeType": "MODIFIED"}]
    }
  ]
}
```

- `authorUserId`는 미확인이어도 `null`로 항상 보낸다. 중앙은 본인 외 userId를 거부한다.
- `changeType`은 `ADDED`·`MODIFIED`·`DELETED`·`RENAMED`다. `CommitChange::from_status`는 `git log --name-status` 상태 문자를 바꾼다. 복사(`C`)는 `ADDED`, 형식 변경(`T`)은 `MODIFIED`이고 그 밖의 문자는 건너뛴다. 내용이 같은 이름 변경은 새 경로에 `RENAMED`로, 내용까지 바뀐 이름 변경은 삭제·추가로 기록한다.
- 한 커밋 안의 같은 경로는 마지막 변경 하나만 남긴다. 중앙은 중복 경로를 거부한다.
- `CommitBatch::split`은 순서를 유지하며 커밋 100개, 본문 128KiB 한도로 나눈다. 한 커밋만으로 128KiB를 넘으면 그 커밋은 배치에 넣지 않고 SHA를 따로 돌려준다.

## 작업 파일

IDX-3-02 본문은 `{"branch": "...", "files": [{"relativePath", "changeType"}]}`이다. 분리된 HEAD나 UTF-8로 해석할 수 없는 브랜치는 `branch: null`이고 빈 목록도 관측으로 보낸다. `changeType`은 `ADDED`·`MODIFIED`·`DELETED`·`UNTRACKED`·`UNMERGED`·`TYPE_CHANGED`이며 `WorkingChange::from_porcelain`이 `git status --porcelain=v1` 두 글자를 바꾼다. 작업 파일 경로는 500코드포인트와 UTF-8 1,000바이트 한도를 모두 만족해야 한다. 초과 경로만 제외하고 정상 파일은 유지한다. 한 번에 보내는 파일은 2,000개 이하다. 넘으면 추적 중인 파일의 변경을 먼저 넣고 미추적 파일로 남는 자리를 채운다. 브랜치는 중앙 규칙(문자·숫자·`._/-`, 255코드포인트 이하, `-` 시작·`..` 금지)을 따르며 `WorkingFiles::branch_allowed`로 확인한다. 수집기는 [작업 파일 수집](working-files.md)이다.

## 검증

`cargo test -p devlink-broker --lib git_scope`

- 직렬화 결과의 키가 허용 목록과 정확히 같고, 절대 경로·저장소 폴더 이름·원격 URL·호스트·작성자 이메일·diff 내용이 본문에 없다.
- 상대 경로 허용·거부 사례, 루트 밖 절대 경로 거부, 변경 유형 매핑, SHA·시각 형식, 중복 경로, 커밋 수·본문 크기 분할, 작업 파일 본문.
- 2026-11-10 로컬 중앙(server develop `d8be28d`, Docker)에 이 저장소의 병합 제외 최근 커밋 150개(파일 1,379개, 네 변경 유형 모두)를 `git_scope`로 직렬화해 보냈다. 100·50개 두 배치로 나뉘었고(최대 약 86KiB) 모두 저장됐다. 같은 배치 재전송은 중복 100건, 작성자 미확인(`null`) 30건도 저장됐다. 같은 본문에 작성자 이메일·diff 필드나 절대 경로, URL 원문 `repoKey`를 넣으면 400, 다른 사용자 작성자는 403이었다.
- 실제 Git 수집 주기·체크포인트는 [커밋 이력 동기화](commit-sync.md)(-185), 작업 파일 업로드는 [작업 파일 수집](working-files.md)(-186)이 맡는다.
