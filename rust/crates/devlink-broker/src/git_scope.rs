//! IDX-3-03 Git collection scope: upload bodies carry only allowed fields.
//!
//! Allowed: repository remote hash (`repoKey`), branch, repository-relative file path, change
//! type, commit SHA and time, and the author as the caller's own user ID or unverified (`null`).
//! Excluded: absolute paths, diffs or file contents, raw remote URLs and raw author emails. These
//! types have no field that could hold such values, and constructors accept validated values only.
//! Commit history (IDX-3-01) and working-file (IDX-3-02) uploaders build their bodies from here.
use crate::git_identity::AuthorAttribution;
use anyhow::{Result, anyhow, bail, ensure};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    path::{Component, Path},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// Central `POST /teams/{teamId}/git/commits:batch` limits.
pub const MAX_COMMITS_PER_BATCH: usize = 100;
pub const MAX_BATCH_BYTES: usize = 128 * 1024;
/// Central `relativePath` limit, counted in UTF-16 code units like Java `String.length`.
pub const MAX_PATH_UTF16: usize = 500;
pub const MAX_WORKING_PATH_BYTES: usize = 1000;
pub const MAX_WORKING_FILES: usize = 2000;
const MAX_BRANCH_CHARS: usize = 255;

/// SHA-256 of the normalized remote URL (`dependency_repo_key`). The raw URL never leaves the PC.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RepoKey(String);

impl RepoKey {
    pub fn new(value: &str) -> Result<Self> {
        ensure!(
            crate::agent_recommendations::repo_key(value),
            "Repository key must be a lowercase SHA-256 hex digest"
        );
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// `/`-separated path inside the repository, following the central validator.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct RelativePath(String);

impl RelativePath {
    pub fn new(path: &str) -> Result<Self> {
        ensure!(valid_relative_path(path), "Path is not repository-relative");
        Ok(Self(path.to_owned()))
    }

    /// Converts an absolute path under `root` and rejects anything outside the repository.
    pub fn from_absolute(root: &Path, path: &Path) -> Result<Self> {
        ensure!(
            root.is_absolute() && path.is_absolute(),
            "Absolute repository and file paths are required"
        );
        let relative = path
            .strip_prefix(root)
            .map_err(|_| anyhow!("Path is outside the repository"))?;
        let mut parts = Vec::new();
        for component in relative.components() {
            let Component::Normal(part) = component else {
                bail!("Path is outside the repository");
            };
            parts.push(
                part.to_str()
                    .ok_or_else(|| anyhow!("Path is not Unicode"))?,
            );
        }
        Self::new(&parts.join("/"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn valid_relative_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    !path.trim().is_empty()
        && path.encode_utf16().count() <= MAX_PATH_UTF16
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && !(bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

/// Commit author: the caller's own user ID or unverified. Takes only the SET-3-03 attribution
/// result, so an email can never reach an upload body.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Author(Option<u64>);

impl Author {
    pub const UNVERIFIED: Self = Self(None);

    pub fn from_attribution(attribution: &AuthorAttribution) -> Self {
        Self(
            attribution
                .author_user_id
                .as_deref()
                .and_then(|id| id.parse::<u64>().ok())
                .filter(|id| *id > 0 && *id <= i64::MAX as u64),
        )
    }

    pub fn user_id(self) -> Option<u64> {
        self.0
    }
}

/// `commit_files.change_type` values accepted by central.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CommitChange {
    Added,
    Modified,
    Deleted,
    Renamed,
}

impl CommitChange {
    /// `git log --name-status` status letter. A copy is a new file and a type change a
    /// modification; unknown letters are skipped by the caller.
    pub fn from_status(status: &str) -> Option<Self> {
        match status.as_bytes().first()? {
            b'A' | b'C' => Some(Self::Added),
            b'M' | b'T' => Some(Self::Modified),
            b'D' => Some(Self::Deleted),
            b'R' => Some(Self::Renamed),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChange {
    relative_path: RelativePath,
    change_type: CommitChange,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitRecord {
    commit_sha: String,
    committed_at: String,
    /// Always serialized; `null` means unverified.
    author_user_id: Author,
    files: Vec<FileChange>,
}

impl CommitRecord {
    pub fn new(
        commit_sha: &str,
        committed_at: &str,
        author: Author,
        files: impl IntoIterator<Item = (RelativePath, CommitChange)>,
    ) -> Result<Self> {
        ensure!(valid_commit_sha(commit_sha), "Invalid commit SHA");
        ensure!(valid_committed_at(committed_at), "Invalid commit time");
        // Central rejects a repeated path in one commit; the last change wins.
        let files: BTreeMap<_, _> = files.into_iter().collect();
        Ok(Self {
            commit_sha: commit_sha.to_owned(),
            committed_at: committed_at.to_owned(),
            author_user_id: author,
            files: files
                .into_iter()
                .map(|(relative_path, change_type)| FileChange {
                    relative_path,
                    change_type,
                })
                .collect(),
        })
    }

    pub fn commit_sha(&self) -> &str {
        &self.commit_sha
    }
}

/// Lowercase 40 (SHA-1) or 64 (SHA-256) hex digits, as central requires.
fn valid_commit_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Central accepts `yyyy-MM-ddTHH:mm:ss[.f{1,6}](Z|±hh:mm)` that also parses as an offset time.
fn valid_committed_at(value: &str) -> bool {
    let bytes = value.as_bytes();
    let digits = |from: usize, to: usize| {
        bytes
            .get(from..to)
            .is_some_and(|part| part.iter().all(u8::is_ascii_digit))
    };
    let at = |index: usize, expected: u8| bytes.get(index) == Some(&expected);
    if !(digits(0, 4)
        && at(4, b'-')
        && digits(5, 7)
        && at(7, b'-')
        && digits(8, 10)
        && at(10, b'T')
        && digits(11, 13)
        && at(13, b':')
        && digits(14, 16)
        && at(16, b':')
        && digits(17, 19))
    {
        return false;
    }
    let mut rest = &value[19..];
    if let Some(fraction) = rest.strip_prefix('.') {
        let count = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if !(1..=6).contains(&count) {
            return false;
        }
        rest = &fraction[count..];
    }
    let zone = rest.as_bytes();
    let offset = zone.len() == 6
        && matches!(zone[0], b'+' | b'-')
        && zone[1..3].iter().all(u8::is_ascii_digit)
        && zone[3] == b':'
        && zone[4..6].iter().all(u8::is_ascii_digit);
    (rest == "Z" || offset) && OffsetDateTime::parse(value, &Rfc3339).is_ok()
}

/// One `commits:batch` request body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitBatch {
    repo_key: RepoKey,
    commits: Vec<CommitRecord>,
}

impl CommitBatch {
    /// Splits in order to the central limits. A commit that alone exceeds the body limit is
    /// never truncated: it is left out and its SHA returned, since no batch can ever carry it.
    pub fn split(
        repo_key: &RepoKey,
        commits: Vec<CommitRecord>,
    ) -> Result<(Vec<Self>, Vec<String>)> {
        let empty = || Self {
            repo_key: repo_key.clone(),
            commits: Vec::new(),
        };
        let mut batches = Vec::new();
        let mut oversized = Vec::new();
        let mut current = empty();
        for commit in commits {
            let alone = Self {
                repo_key: repo_key.clone(),
                commits: vec![commit],
            };
            if alone.body_len()? > MAX_BATCH_BYTES {
                oversized.extend(alone.commits.into_iter().map(|c| c.commit_sha));
                continue;
            }
            current.commits.extend(alone.commits);
            if current.commits.len() <= MAX_COMMITS_PER_BATCH
                && current.body_len()? <= MAX_BATCH_BYTES
            {
                continue;
            }
            let overflow = current.commits.pop().expect("a commit was just added");
            batches.push(std::mem::replace(&mut current, empty()));
            current.commits.push(overflow);
        }
        if !current.commits.is_empty() {
            batches.push(current);
        }
        Ok((batches, oversized))
    }

    pub fn commits(&self) -> &[CommitRecord] {
        &self.commits
    }

    fn body_len(&self) -> Result<usize> {
        Ok(serde_json::to_vec(self)?.len())
    }
}

/// Working-tree change kinds of the IDX-3-02 working-file body.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WorkingChange {
    Added,
    Modified,
    Deleted,
    Untracked,
    Unmerged,
    TypeChanged,
}

impl WorkingChange {
    /// The two status letters of `git status --porcelain=v1`.
    pub fn from_porcelain(flags: [u8; 2]) -> Self {
        match &flags {
            b"??" => Self::Untracked,
            b"DD" | b"AU" | b"UD" | b"UA" | b"DU" | b"AA" | b"UU" => Self::Unmerged,
            _ if flags.contains(&b'D') => Self::Deleted,
            _ if flags.contains(&b'A') => Self::Added,
            _ if flags.contains(&b'T') => Self::TypeChanged,
            _ => Self::Modified,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkingFile {
    relative_path: RelativePath,
    change_type: WorkingChange,
}

/// Current branch (`null` on a detached HEAD) and uncommitted files. An empty list is still an
/// observation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkingFiles {
    branch: Option<String>,
    files: Vec<WorkingFile>,
}

impl WorkingFiles {
    /// 작업 파일에만 적용되는 중앙 UTF-8 바이트 한도 검사
    pub fn path_allowed(path: &RelativePath) -> bool {
        path.as_str().len() <= MAX_WORKING_PATH_BYTES
    }

    /// Central branch rule: letters, digits and `._/-`, at most 255 UTF-16 units, no leading `-`
    /// and no `..`. A collector sends `null` rather than an unaccepted name.
    pub fn branch_allowed(branch: &str) -> bool {
        !branch.is_empty()
            && branch.encode_utf16().count() <= MAX_BRANCH_CHARS
            && !branch.starts_with('-')
            && !branch.contains("..")
            && branch
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
    }

    pub fn new(
        branch: Option<&str>,
        files: impl IntoIterator<Item = (RelativePath, WorkingChange)>,
    ) -> Result<Self> {
        if let Some(branch) = branch {
            ensure!(Self::branch_allowed(branch), "Invalid branch name");
        }
        let files: BTreeMap<_, _> = files
            .into_iter()
            .filter(|(path, _)| Self::path_allowed(path))
            .collect();
        ensure!(files.len() <= MAX_WORKING_FILES, "Too many changed files");
        Ok(Self {
            branch: branch.map(str::to_owned),
            files: files
                .into_iter()
                .map(|(relative_path, change_type)| WorkingFile {
                    relative_path,
                    change_type,
                })
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use std::{collections::BTreeSet, path::PathBuf};

    const SHA1: &str = "0123456789abcdef0123456789abcdef01234567";
    const TIME: &str = "2026-11-10T12:34:56+09:00";

    fn keys(value: &Value, found: &mut BTreeSet<String>) {
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    found.insert(key.clone());
                    keys(value, found);
                }
            }
            Value::Array(items) => items.iter().for_each(|item| keys(item, found)),
            _ => {}
        }
    }

    fn key_set(value: &Value) -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        keys(value, &mut found);
        found
    }

    fn path(value: &str) -> RelativePath {
        RelativePath::new(value).unwrap()
    }

    fn repo_key() -> RepoKey {
        RepoKey::new(&"a".repeat(64)).unwrap()
    }

    fn commit(sha: &str) -> CommitRecord {
        CommitRecord::new(
            sha,
            TIME,
            Author::UNVERIFIED,
            [(path("src/a.ts"), CommitChange::Modified)],
        )
        .unwrap()
    }

    #[test]
    fn commit_batch_serializes_only_the_allowed_fields_and_never_private_sources() {
        let root = std::env::temp_dir().join("devlink-private-project");
        let absolute = root.join("src").join("login.ts");
        let remote = "https://github.com/acme/secret-repo.git";
        let key = format!("{:x}", Sha256::digest(b"github.com/acme/secret-repo"));
        let author_email = "dev.private@example.com";
        let diff = "@@ -1 +1 @@\n-password=1\n+password=2";
        let commit = CommitRecord::new(
            SHA1,
            TIME,
            Author::from_attribution(&AuthorAttribution {
                author_user_id: None,
            }),
            [(
                RelativePath::from_absolute(&root, &absolute).unwrap(),
                CommitChange::Modified,
            )],
        )
        .unwrap();
        let (batches, _) = CommitBatch::split(&RepoKey::new(&key).unwrap(), vec![commit]).unwrap();
        let body = serde_json::to_value(&batches[0]).unwrap();
        assert_eq!(
            body,
            json!({"repoKey": key, "commits": [{
                "commitSha": SHA1, "committedAt": TIME, "authorUserId": null,
                "files": [{"relativePath": "src/login.ts", "changeType": "MODIFIED"}]
            }]})
        );
        assert_eq!(
            key_set(&body),
            [
                "repoKey",
                "commits",
                "commitSha",
                "committedAt",
                "authorUserId",
                "files",
                "relativePath",
                "changeType"
            ]
            .map(String::from)
            .into()
        );
        let text = body.to_string();
        for private in [
            root.to_str().unwrap(),
            "devlink-private-project",
            remote,
            "github.com",
            "secret-repo",
            author_email,
            "example.com",
            diff,
            "password",
        ] {
            assert!(!text.contains(private), "{private} leaked into {text}");
        }
    }

    #[test]
    fn author_is_own_user_id_or_unverified_null() {
        let author = |id: Option<&str>| {
            Author::from_attribution(&AuthorAttribution {
                author_user_id: id.map(str::to_owned),
            })
        };
        assert_eq!(author(Some("51")).user_id(), Some(51));
        for invalid in [
            None,
            Some("0"),
            Some("-1"),
            Some("abc"),
            Some("9223372036854775808"),
        ] {
            assert_eq!(author(invalid), Author::UNVERIFIED);
        }
        let own = CommitRecord::new(SHA1, TIME, author(Some("51")), []).unwrap();
        assert_eq!(serde_json::to_value(own).unwrap()["authorUserId"], 51);
        let unknown = serde_json::to_value(commit(SHA1)).unwrap();
        assert!(unknown.as_object().unwrap().contains_key("authorUserId"));
        assert_eq!(unknown["authorUserId"], Value::Null);
    }

    #[test]
    fn relative_paths_follow_the_central_validator() {
        for valid in ["a", "src/a.ts", "docs/한글 파일.md", "a/.env", "x..y/z"] {
            assert!(RelativePath::new(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "",
            " ",
            "/etc/passwd",
            "C:/work/a.ts",
            "c:relative",
            "src\\a.ts",
            "\\\\server\\share",
            "src//a.ts",
            "src/",
            "./a",
            "a/../b",
            "..",
            "a\0b",
            "a\nb",
        ] {
            assert!(RelativePath::new(invalid).is_err(), "{invalid:?}");
        }
        let limit = format!("a/{}", "x".repeat(MAX_PATH_UTF16 - 2));
        assert!(RelativePath::new(&limit).is_ok());
        assert!(RelativePath::new(&format!("{limit}x")).is_err());
        // A supplementary character is two UTF-16 units in central's Java length.
        let wide = format!("a/{}😀", "x".repeat(MAX_PATH_UTF16 - 3));
        assert!(RelativePath::new(&wide).is_err());
    }

    #[test]
    fn absolute_paths_become_relative_only_inside_the_repository() {
        let root = std::env::temp_dir().join("devlink-scope-root");
        let inside = root.join("src").join("a.ts");
        assert_eq!(
            RelativePath::from_absolute(&root, &inside)
                .unwrap()
                .as_str(),
            "src/a.ts"
        );
        for outside in [
            root.clone(),
            std::env::temp_dir().join("other").join("a.ts"),
            root.join("..").join("escape.ts"),
        ] {
            assert!(
                RelativePath::from_absolute(&root, &outside).is_err(),
                "{outside:?}"
            );
        }
        assert!(RelativePath::from_absolute(Path::new("relative"), &inside).is_err());
        assert!(RelativePath::from_absolute(&root, &PathBuf::from("src/a.ts")).is_err());
    }

    #[test]
    fn change_types_map_to_central_values() {
        for (status, expected) in [
            ("A", CommitChange::Added),
            ("C75", CommitChange::Added),
            ("M", CommitChange::Modified),
            ("T", CommitChange::Modified),
            ("D", CommitChange::Deleted),
            ("R100", CommitChange::Renamed),
        ] {
            assert_eq!(
                CommitChange::from_status(status),
                Some(expected),
                "{status}"
            );
        }
        for skipped in ["", "U", "X", "?"] {
            assert_eq!(CommitChange::from_status(skipped), None);
        }
        assert_eq!(
            serde_json::to_value([
                CommitChange::Added,
                CommitChange::Modified,
                CommitChange::Deleted,
                CommitChange::Renamed
            ])
            .unwrap(),
            json!(["ADDED", "MODIFIED", "DELETED", "RENAMED"])
        );
    }

    #[test]
    fn commit_sha_and_time_follow_the_central_format() {
        let sha256 = "a".repeat(64);
        assert!(CommitRecord::new(&sha256, TIME, Author::UNVERIFIED, []).is_ok());
        for sha in [&SHA1.to_uppercase(), &"a".repeat(39), &"g".repeat(40), ""] {
            assert!(CommitRecord::new(sha, TIME, Author::UNVERIFIED, []).is_err());
        }
        for time in ["2026-11-10T12:34:56Z", "2026-11-10T12:34:56.123456-05:30"] {
            assert!(
                CommitRecord::new(SHA1, time, Author::UNVERIFIED, []).is_ok(),
                "{time}"
            );
        }
        for time in [
            "2026-11-10 12:34:56+09:00",
            "2026-11-10T12:34:56",
            "2026-11-10T12:34:56.1234567Z",
            "2026-11-10t12:34:56z",
            "2026-13-10T12:34:56Z",
            "2026-11-10T12:34:56+0900",
        ] {
            assert!(
                CommitRecord::new(SHA1, time, Author::UNVERIFIED, []).is_err(),
                "{time}"
            );
        }
    }

    #[test]
    fn repeated_paths_in_one_commit_keep_the_last_change() {
        let record = CommitRecord::new(
            SHA1,
            TIME,
            Author::UNVERIFIED,
            [
                (path("b.ts"), CommitChange::Added),
                (path("a.ts"), CommitChange::Modified),
                (path("b.ts"), CommitChange::Deleted),
            ],
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(record).unwrap()["files"],
            json!([
                {"relativePath": "a.ts", "changeType": "MODIFIED"},
                {"relativePath": "b.ts", "changeType": "DELETED"}
            ])
        );
        assert!(RepoKey::new(&"A".repeat(64)).is_err());
        assert!(RepoKey::new("https://github.com/acme/repo.git").is_err());
    }

    #[test]
    fn batches_respect_commit_count_and_body_size_in_order() {
        let shas: Vec<String> = (0..250).map(|n| format!("{n:040x}")).collect();
        let (batches, oversized) =
            CommitBatch::split(&repo_key(), shas.iter().map(|sha| commit(sha)).collect()).unwrap();
        assert!(oversized.is_empty());
        assert_eq!(
            batches
                .iter()
                .map(|b| b.commits().len())
                .collect::<Vec<_>>(),
            [100, 100, 50]
        );
        let order: Vec<_> = batches
            .iter()
            .flat_map(|b| b.commits().iter().map(CommitRecord::commit_sha))
            .collect();
        assert_eq!(order, shas.iter().map(String::as_str).collect::<Vec<_>>());
        assert!(
            CommitBatch::split(&repo_key(), Vec::new())
                .unwrap()
                .0
                .is_empty()
        );

        // About 50 KiB per commit: two fit in one body, the third starts a new one.
        let large = |n: usize| {
            CommitRecord::new(
                &format!("{n:040x}"),
                TIME,
                Author::UNVERIFIED,
                (0..120).map(|i| {
                    (
                        path(&format!("dir/{n}/{i}/{}", "x".repeat(380))),
                        CommitChange::Added,
                    )
                }),
            )
            .unwrap()
        };
        let (batches, _) = CommitBatch::split(&repo_key(), (0..5).map(large).collect()).unwrap();
        assert!(batches.len() > 1);
        for batch in &batches {
            assert!(serde_json::to_vec(batch).unwrap().len() <= MAX_BATCH_BYTES);
        }
        assert_eq!(batches.iter().map(|b| b.commits().len()).sum::<usize>(), 5);

        let oversized = CommitRecord::new(
            SHA1,
            TIME,
            Author::UNVERIFIED,
            (0..400).map(|i| {
                (
                    path(&format!("{i}/{}", "x".repeat(400))),
                    CommitChange::Added,
                )
            }),
        )
        .unwrap();
        // A commit no body can carry is left out; the others still go in order.
        let (batches, skipped) = CommitBatch::split(
            &repo_key(),
            vec![commit(&"b".repeat(40)), oversized, commit(&"c".repeat(40))],
        )
        .unwrap();
        assert_eq!(skipped, [SHA1]);
        assert_eq!(
            batches[0]
                .commits()
                .iter()
                .map(CommitRecord::commit_sha)
                .collect::<Vec<_>>(),
            ["b".repeat(40), "c".repeat(40)]
        );
    }

    #[test]
    fn working_files_carry_branch_relative_paths_and_change_types_only() {
        let body = serde_json::to_value(
            WorkingFiles::new(
                Some("feature/login"),
                [
                    (path("src/b.ts"), WorkingChange::from_porcelain(*b" M")),
                    (path("src/a.ts"), WorkingChange::from_porcelain(*b"??")),
                    (path("src/c.ts"), WorkingChange::from_porcelain(*b"UU")),
                    (path("src/d.ts"), WorkingChange::from_porcelain(*b" T")),
                ],
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            body,
            json!({"branch": "feature/login", "files": [
                {"relativePath": "src/a.ts", "changeType": "UNTRACKED"},
                {"relativePath": "src/b.ts", "changeType": "MODIFIED"},
                {"relativePath": "src/c.ts", "changeType": "UNMERGED"},
                {"relativePath": "src/d.ts", "changeType": "TYPE_CHANGED"}
            ]})
        );
        assert_eq!(
            key_set(&body),
            ["branch", "files", "relativePath", "changeType"]
                .map(String::from)
                .into()
        );
        let detached = serde_json::to_value(WorkingFiles::new(None, []).unwrap()).unwrap();
        assert_eq!(detached, json!({"branch": null, "files": []}));
        for branch in ["main", "feature/login-2", "release_1.2", "기능/로그인"] {
            assert!(WorkingFiles::new(Some(branch), []).is_ok(), "{branch:?}");
        }
        for branch in [
            "",
            " ",
            "a\nb",
            "-main",
            "a..b",
            "fix#12",
            "user@x",
            "a b",
            &"b".repeat(MAX_BRANCH_CHARS + 1),
        ] {
            assert!(WorkingFiles::new(Some(branch), []).is_err(), "{branch:?}");
        }
        let many = (0..=MAX_WORKING_FILES).map(|i| (path(&format!("f{i}")), WorkingChange::Added));
        assert!(WorkingFiles::new(None, many).is_err());
        assert_eq!(
            WorkingChange::from_porcelain(*b"D "),
            WorkingChange::Deleted
        );
        assert_eq!(WorkingChange::from_porcelain(*b"A "), WorkingChange::Added);
    }

    #[test]
    fn working_paths_enforce_bytes_without_discarding_other_files_or_commit_paths() {
        let boundary = format!("{}/{}", "가".repeat(166), "나".repeat(167));
        let oversized = format!("{}/{}", "가".repeat(167), "나".repeat(167));
        assert_eq!(boundary.len(), MAX_WORKING_PATH_BYTES);
        assert_eq!(oversized.len(), MAX_WORKING_PATH_BYTES + 3);
        let allowed = path(&boundary);
        let too_long = path(&oversized);
        assert!(WorkingFiles::path_allowed(&allowed));
        assert!(!WorkingFiles::path_allowed(&too_long));
        let body = serde_json::to_value(
            WorkingFiles::new(
                None,
                [
                    (allowed, WorkingChange::Added),
                    (too_long.clone(), WorkingChange::Modified),
                    (path("src/normal.ts"), WorkingChange::Untracked),
                ],
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(body["files"].as_array().unwrap().len(), 2);
        assert!(body["files"].as_array().unwrap().iter().all(|file| {
            file["relativePath"].as_str().unwrap().len() <= MAX_WORKING_PATH_BYTES
        }));
        let commit = CommitRecord::new(
            SHA1,
            TIME,
            Author::UNVERIFIED,
            [(too_long, CommitChange::Added)],
        )
        .unwrap();
        let commit = serde_json::to_value(commit).unwrap();
        assert_eq!(commit["files"][0]["relativePath"], oversized);
    }
}
