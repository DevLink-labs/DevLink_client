use crate::git_scope::{RelativePath, WorkingChange};
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::timeout,
};

const OUTPUT_LIMIT: usize = 16 * 1024 * 1024;

pub(crate) struct GitProject {
    root: PathBuf,
    remote: String,
    pub key: String,
}

pub(super) struct SourceFile {
    pub path: String,
    pub text: String,
}

/// One non-merge commit from `git log --name-status`. The author email stays on this PC; the
/// commit uploader only uses it for SET-3-03 attribution.
pub(crate) struct RawCommit {
    pub sha: String,
    pub committed_at: String,
    pub author_email: String,
    /// `(status, path)`; for renames and copies the path is the new one.
    pub files: Vec<(String, String)>,
}

impl GitProject {
    pub(crate) async fn open(path: &Path, remote: &str) -> Result<Self> {
        ensure!(
            !remote.is_empty()
                && !remote.starts_with('-')
                && remote
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
            "Invalid Git remote name"
        );
        let root = run(path, &["rev-parse", "--show-toplevel"], None, 4096).await?;
        let root = PathBuf::from(std::str::from_utf8(&root)?.trim_end_matches(['\r', '\n']));
        let url = run(&root, &["remote", "get-url", remote], None, 8192).await?;
        let canonical = remote_identity(std::str::from_utf8(&url)?.trim_end_matches(['\r', '\n']))?;
        let key = format!("{:x}", Sha256::digest(canonical.as_bytes()));
        Ok(Self {
            root,
            remote: remote.to_owned(),
            key,
        })
    }

    pub(super) async fn sha(&self, branch: &str) -> Result<String> {
        let reference = format!("refs/remotes/{}/{branch}^{{commit}}", self.remote);
        let result = run(
            &self.root,
            &["rev-parse", "--verify", &reference],
            None,
            128,
        )
        .await?;
        let sha = std::str::from_utf8(&result)?.trim();
        ensure!(
            (sha.len() == 40 || sha.len() == 64) && sha.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid Git commit ID"
        );
        Ok(sha.to_owned())
    }

    pub(super) async fn require_ancestor(&self, old: &str, new: &str) -> Result<()> {
        run(
            &self.root,
            &["merge-base", "--is-ancestor", old, new],
            None,
            128,
        )
        .await?;
        Ok(())
    }

    /// Current `HEAD` commit, or `None` before the first commit.
    pub(crate) async fn head(&self) -> Result<Option<String>> {
        let Ok(result) = run(
            &self.root,
            &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
            None,
            128,
        )
        .await
        else {
            return Ok(None);
        };
        let sha = std::str::from_utf8(&result)?.trim();
        ensure!(valid_sha(sha), "Invalid Git commit ID");
        Ok(Some(sha.to_owned()))
    }

    /// Non-merge commits reachable from `tip` but not from `since`, committed at or after the
    /// Unix time `after`, oldest first. Topological order puts every ancestor of a commit before
    /// it, so the range up to any listed commit can be checkpointed by that commit.
    ///
    /// The date only filters: `--since` would stop at a commit with a skewed old date and drop
    /// its recent ancestors. Git before 2.37 lacks `--since-as-filter` and falls back to it.
    pub(crate) async fn commit_ids(
        &self,
        since: Option<&str>,
        tip: &str,
        after: u64,
    ) -> Result<Vec<String>> {
        ensure!(
            valid_sha(tip) && since.is_none_or(valid_sha),
            "Invalid Git commit ID"
        );
        let range = since.map_or_else(|| tip.to_owned(), |old| format!("{old}..{tip}"));
        let list = |date: String| {
            let range = range.clone();
            async move {
                let args = [
                    "rev-list",
                    "--no-merges",
                    "--topo-order",
                    "--reverse",
                    &date,
                    &range,
                    "--",
                ];
                run(&self.root, &args, None, OUTPUT_LIMIT).await
            }
        };
        let output = match list(format!("--since-as-filter=@{after}")).await {
            Ok(output) => output,
            Err(_) => list(format!("--since=@{after}")).await?,
        };
        let ids: Vec<String> = std::str::from_utf8(&output)?
            .lines()
            .map(str::to_owned)
            .collect();
        ensure!(ids.iter().all(|id| valid_sha(id)), "Invalid Git commit ID");
        Ok(ids)
    }

    /// The listed commits in the given order. Root commits always list their files and rename
    /// detection is fixed (`-M`, 50% similarity, limit 1000) instead of following user
    /// configuration, so every clone reports the same files for the same SHA.
    pub(crate) async fn commits(&self, ids: &[String]) -> Result<Vec<RawCommit>> {
        ensure!(ids.iter().all(|id| valid_sha(id)), "Invalid Git commit ID");
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let input = ids.iter().flat_map(|id| [id.as_bytes(), b"\n"]).flatten();
        let output = run(
            &self.root,
            &[
                "-c",
                "core.quotePath=false",
                "log",
                "--stdin",
                "--no-walk=unsorted",
                "--root",
                "--no-ext-diff",
                "--no-color",
                "--no-show-signature",
                "-M",
                "-l1000",
                "--name-status",
                "-z",
                "--format=%x01%H%x00%cI%x00%ae",
                "--",
            ],
            Some(input.copied().collect()),
            OUTPUT_LIMIT,
        )
        .await?;
        parse_log(&output)
    }

    pub(super) async fn snapshot(&self, sha: &str) -> Result<Vec<SourceFile>> {
        let listing = run(
            &self.root,
            &["ls-tree", "-r", "-z", "--full-tree", sha],
            None,
            OUTPUT_LIMIT,
        )
        .await?;
        let mut paths = Vec::new();
        let mut requests = Vec::new();
        for entry in listing
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            let Some(separator) = entry.iter().position(|byte| *byte == b'\t') else {
                bail!("Invalid Git tree entry")
            };
            let header = std::str::from_utf8(&entry[..separator])?;
            let Ok(path) = std::str::from_utf8(&entry[separator + 1..]) else {
                continue;
            };
            let Some(oid) = header
                .strip_prefix("100644 blob ")
                .or_else(|| header.strip_prefix("100755 blob "))
            else {
                continue;
            };
            if !valid_path(path) || !source_path(path) {
                continue;
            }
            ensure!(paths.len() < 5000, "Too many source files");
            requests.extend_from_slice(oid.as_bytes());
            requests.push(b'\n');
            paths.push((path, oid));
        }
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let bytes = run(
            &self.root,
            &["cat-file", "--batch"],
            Some(requests),
            OUTPUT_LIMIT,
        )
        .await?;
        let mut position = 0;
        let mut files = Vec::with_capacity(paths.len());
        for (path, oid) in paths {
            let rest = bytes
                .get(position..)
                .context("Git blob header is missing")?;
            let newline = rest
                .iter()
                .position(|byte| *byte == b'\n')
                .context("Invalid Git blob header")?;
            let header = std::str::from_utf8(&rest[..newline])?;
            let size: usize = header
                .strip_prefix(oid)
                .and_then(|tail| tail.strip_prefix(" blob "))
                .context("Git blob is missing")?
                .parse()?;
            let start = position + newline + 1;
            let end = start.checked_add(size).context("Invalid Git blob size")?;
            ensure!(bytes.get(end) == Some(&b'\n'), "Invalid Git blob boundary");
            if let Ok(text) = std::str::from_utf8(&bytes[start..end]) {
                files.push(SourceFile {
                    path: path.to_owned(),
                    text: text.to_owned(),
                });
            }
            position = end + 1;
        }
        ensure!(position == bytes.len(), "Unexpected Git blob data");
        Ok(files)
    }

    /// Current branch (`None` on a detached HEAD) and the working-tree changes, following the
    /// IDX-3-02 working-file contract: at most 2,000 distinct paths. Paths outside the central
    /// path rules are skipped.
    pub(crate) async fn working(
        &self,
    ) -> Result<(Option<String>, Vec<(RelativePath, WorkingChange)>)> {
        let branch = run(&self.root, &["branch", "--show-current"], None, 1024).await?;
        let branch = std::str::from_utf8(&branch)?.trim();
        let branch = (!branch.is_empty()).then(|| branch.to_owned());
        let status = run(
            &self.root,
            &[
                "--no-optional-locks",
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--no-renames",
            ],
            None,
            OUTPUT_LIMIT,
        )
        .await?;
        Ok((branch, parse_status(&status)?))
    }
}

fn valid_sha(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `--format=%x01%H%x00%cI%x00%ae --name-status -z`: each commit is `\x01SHA\0TIME\0EMAIL\0`
/// (the email may be empty), then `\n` and NUL-separated status/path entries where renames and
/// copies carry the old and the new path.
fn parse_log(output: &[u8]) -> Result<Vec<RawCommit>> {
    let mut commits = Vec::new();
    for entry in output.split(|b| *b == 1).filter(|e| !e.is_empty()) {
        let mut fields = entry.split(|b| *b == 0);
        let (Some(sha), Some(time), Some(email)) = (fields.next(), fields.next(), fields.next())
        else {
            bail!("Invalid Git log entry");
        };
        let sha = std::str::from_utf8(sha).context("Invalid Git commit ID")?;
        ensure!(valid_sha(sha), "Invalid Git commit ID");
        let mut rest = fields
            .map(|field| field.strip_prefix(b"\n").unwrap_or(field))
            .filter(|field| !field.is_empty());
        let mut files = Vec::new();
        while let Some(status) = rest.next() {
            let status = std::str::from_utf8(status).context("Invalid Git status")?;
            if matches!(status.as_bytes().first(), Some(b'R' | b'C')) {
                rest.next().context("Rename without source path")?;
            }
            let path = rest.next().context("Status without path")?;
            // Central cannot take a non-UTF-8 path; skip that file only.
            if let Ok(path) = std::str::from_utf8(path) {
                files.push((status.to_owned(), path.to_owned()));
            }
        }
        commits.push(RawCommit {
            sha: sha.to_owned(),
            committed_at: std::str::from_utf8(time)
                .context("Invalid commit time")?
                .to_owned(),
            // Only compared locally for attribution, so a lossy decode is harmless.
            author_email: String::from_utf8_lossy(email).into_owned(),
            files,
        });
    }
    Ok(commits)
}

/// `git status --porcelain=v1 -z --no-renames` records: two status letters, a space, the path.
fn parse_status(status: &[u8]) -> Result<Vec<(RelativePath, WorkingChange)>> {
    let mut files = std::collections::BTreeMap::new();
    for record in status
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        if record.len() < 4 || record[2] != b' ' {
            bail!("Invalid Git status entry");
        }
        let Ok(path) = std::str::from_utf8(&record[3..]) else {
            continue;
        };
        let Ok(path) = RelativePath::new(path) else {
            continue;
        };
        files.insert(path, WorkingChange::from_porcelain([record[0], record[1]]));
        ensure!(
            files.len() <= crate::git_scope::MAX_WORKING_FILES,
            "Too many changed files"
        );
    }
    Ok(files.into_iter().collect())
}

pub(super) fn valid_path(path: &str) -> bool {
    !path.trim().is_empty()
        && path.len() <= 1000
        && path.chars().count() <= 500
        && !path.contains('\\')
        && path.as_bytes().get(1) != Some(&b':')
        && !path.chars().any(char::is_control)
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

fn source_path(path: &str) -> bool {
    if path
        .split('/')
        .any(|part| matches!(part, "node_modules" | "target" | "dist" | "build"))
        || path.starts_with("src/test/")
        || path.contains("/src/test/")
    {
        return false;
    }
    matches!(
        path.rsplit_once('.').map(|(_, suffix)| suffix),
        Some("java" | "ts" | "tsx" | "js" | "jsx")
    ) || path.ends_with(".sql")
        && (path.starts_with("db/migration/") || path.contains("/db/migration/"))
}

fn remote_identity(raw: &str) -> Result<String> {
    let (host, port, path) = if raw.contains("://") {
        let url = url::Url::parse(raw).map_err(|_| anyhow::anyhow!("Unsupported Git remote"))?;
        let host = url
            .host_str()
            .context("Git remote host is missing")?
            .to_ascii_lowercase();
        let default = match url.scheme() {
            "https" => 443,
            "ssh" => 22,
            _ => bail!("Unsupported Git remote scheme"),
        };
        (
            host,
            url.port().filter(|port| *port != default),
            url.path().to_owned(),
        )
    } else {
        let (address, path) = raw.split_once(':').context("Unsupported Git remote")?;
        let host = address.rsplit('@').next().unwrap_or_default();
        ensure!(
            host.contains('.')
                && host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-.".contains(&b)),
            "Unsupported Git remote host"
        );
        (host.to_ascii_lowercase(), None, path.to_owned())
    };
    let path = path
        .strip_prefix('/')
        .unwrap_or(&path)
        .trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    ensure!(
        valid_path(path)
            && path.split('/').count() >= 2
            && !path.bytes().any(|byte| b"%?#".contains(&byte)),
        "Unsupported Git remote path"
    );
    let host = port.map_or(host.clone(), |port| format!("{host}:{port}"));
    Ok(format!("{host}/{path}"))
}

async fn run(root: &Path, args: &[&str], input: Option<Vec<u8>>, limit: usize) -> Result<Vec<u8>> {
    use std::process::Stdio;
    timeout(Duration::from_secs(15), async {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_NO_LAZY_FETCH", "1")
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        // 백그라운드 브로커는 콘솔이 없어서, 플래그 없이 git을 띄우면 3초마다 콘솔 창이 깜박인다.
        #[cfg(windows)]
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        let mut child = command.spawn().context("Cannot start Git")?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().context("Cannot read Git output")?;
        let mut bytes = Vec::new();
        let mut reader = stdout.take((limit + 1) as u64);
        tokio::try_join!(
            async {
                reader.read_to_end(&mut bytes).await?;
                ensure!(bytes.len() <= limit, "Git output exceeds analysis limit");
                Ok::<_, anyhow::Error>(())
            },
            async {
                if let (Some(mut pipe), Some(input)) = (stdin, input) {
                    pipe.write_all(&input).await?;
                    pipe.shutdown().await?;
                }
                Ok::<_, anyhow::Error>(())
            }
        )?;
        ensure!(child.wait().await?.success(), "Git operation failed");
        Ok(bytes)
    })
    .await
    .context("Git operation timed out")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_records_become_sorted_working_changes() {
        let status =
            b" M src/b.ts\0?? \xed\x95\x9c.md\0A  src/a.ts\0 D gone.ts\0UU c.ts\0MM src/b.ts\0";
        let files = parse_status(status).unwrap();
        let names: Vec<_> = files
            .iter()
            .map(|(path, change)| (path.as_str(), *change))
            .collect();
        assert_eq!(
            names,
            [
                ("c.ts", WorkingChange::Unmerged),
                ("gone.ts", WorkingChange::Deleted),
                ("src/a.ts", WorkingChange::Added),
                ("src/b.ts", WorkingChange::Modified),
                ("한.md", WorkingChange::Untracked),
            ]
        );
        assert!(parse_status(b"").unwrap().is_empty());
    }

    #[test]
    fn status_skips_paths_central_rejects_and_bounds_the_list() {
        let files = parse_status(b" M ok.ts\0 M bad\\name.ts\0?? \xff\xfe\0").unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0.as_str(), "ok.ts");
        assert!(parse_status(b"XM\0").is_err());
        assert!(parse_status(b"MMXpath\0").is_err());
        let many: Vec<u8> = (0..=crate::git_scope::MAX_WORKING_FILES)
            .flat_map(|i| format!("?? f{i}\0").into_bytes())
            .collect();
        assert!(parse_status(&many).is_err());
    }

    #[test]
    fn log_entries_keep_the_new_path_of_renames_and_allow_an_empty_email() {
        let first = "c805111b99cc2c3ef848f5809c7e9a2382f8cf89";
        let second = "120ac440db5a24b6c664e27293cdc3581caa4009";
        let mut output = Vec::new();
        for part in [
            format!("\x01{first}\x002026-11-10T14:36:09+09:00\x00dev@example.test\x00").as_bytes(),
            b"\nR100\x00a.txt\x00b c.txt\x00M\x00src/\xed\x95\x9c.ts\x00",
            format!("\x01{second}\x002026-11-10T14:36:09+09:00\x00\x00").as_bytes(),
            b"\nA\x00a.txt\x00D\x00\xff.bin\x00",
        ] {
            output.extend_from_slice(part);
        }
        let commits = parse_log(&output).unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].sha, first);
        assert_eq!(commits[0].author_email, "dev@example.test");
        assert_eq!(
            commits[0].files,
            [
                ("R100".to_owned(), "b c.txt".to_owned()),
                ("M".to_owned(), "src/한.ts".to_owned())
            ]
        );
        assert_eq!(commits[1].committed_at, "2026-11-10T14:36:09+09:00");
        assert_eq!(commits[1].author_email, "");
        // A non-UTF-8 path is dropped; the commit and its other files stay.
        assert_eq!(commits[1].files, [("A".to_owned(), "a.txt".to_owned())]);
        assert!(parse_log(b"").unwrap().is_empty());
        assert!(parse_log(b"\x01not-a-sha\x00t\x00e\x00").is_err());
        assert!(
            parse_log(format!("\x01{first}\x00t\x00e\x00\nR100\x00only-old\x00").as_bytes())
                .is_err()
        );
    }

    #[test]
    fn remote_identity_normalizes_transport_and_host_only() {
        let same = [
            "https://github.com/Acme/Demo.git",
            "https://GitHub.com/Acme/Demo",
            "https://github.com:443/Acme/Demo/",
            "ssh://git@github.com/Acme/Demo.git",
            "ssh://git@github.com:22/Acme/Demo",
            "git@github.com:Acme/Demo.git",
        ];
        for remote in same {
            assert_eq!(
                remote_identity(remote).unwrap(),
                "github.com/Acme/Demo",
                "{remote}"
            );
        }
        assert_eq!(
            remote_identity("https://git.example.com:8443/team/sub/repo.git").unwrap(),
            "git.example.com:8443/team/sub/repo"
        );
        // Path case is kept: these are different repository keys.
        assert_ne!(
            remote_identity("git@github.com:acme/demo.git").unwrap(),
            remote_identity("git@github.com:Acme/Demo.git").unwrap()
        );
        for unsupported in [
            "http://github.com/acme/demo.git",
            "file:///C:/repo",
            "C:/repo",
            "/srv/repo.git",
            "https://github.com/demo",
            "https://github.com/acme/../demo",
            "https://github.com/acme/demo%2Fx",
            "git@localhost:acme/demo.git",
        ] {
            assert!(remote_identity(unsupported).is_err(), "{unsupported}");
        }
    }
}
