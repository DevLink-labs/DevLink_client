use crate::git_scope::{RelativePath, WorkingChange};
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
    time::timeout,
};

const OUTPUT_LIMIT: usize = 16 * 1024 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(15);
/// Root trees per prefetch request in a treeless clone, and the time one request may take.
const TREE_FETCH_BATCH: usize = 200;
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub(crate) struct OutputLimitExceeded;

impl std::fmt::Display for OutputLimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Git output exceeds analysis limit")
    }
}

impl std::error::Error for OutputLimitExceeded {}

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

    /// Git treats these commits as roots even though their actual parents are missing.
    pub(crate) async fn shallow_boundaries(&self) -> Result<BTreeSet<String>> {
        let path = run(
            &self.root,
            &["rev-parse", "--git-path", "shallow"],
            None,
            8192,
        )
        .await?;
        let path = std::str::from_utf8(&path)?.trim_end_matches(['\r', '\n']);
        let contents = match tokio::fs::read_to_string(self.root.join(path)).await {
            Ok(contents) => contents,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(BTreeSet::new()),
            Err(error) => return Err(error).context("Cannot read Git shallow boundaries"),
        };
        contents
            .lines()
            .map(|sha| {
                ensure!(valid_sha(sha), "Invalid Git shallow boundary");
                Ok(sha.to_owned())
            })
            .collect()
    }

    /// Non-merge commits reachable from `tip` but not from `since`, committed at or after the
    /// Unix time `after`, oldest first. Topological order puts every ancestor of a commit before
    /// it, so the range up to any listed commit can be checkpointed by that commit.
    ///
    /// Filter streamed committer timestamps locally, using options available before Git 2.37.
    /// A date traversal cutoff would drop recent ancestors of a commit with a skewed old date.
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
        history_ids(&self.root, &range, after).await
    }

    /// The listed commits in the given order. Detect only exact renames so the result
    /// depends on the trees and not on blob availability in a partial clone.
    pub(crate) async fn commits(&self, ids: &[String]) -> Result<Vec<RawCommit>> {
        ensure!(ids.iter().all(|id| valid_sha(id)), "Invalid Git commit ID");
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(remote) = self.tree_promisor().await {
            self.prefetch_trees(&remote, ids).await?;
        }
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
                "--ignore-submodules=none",
                "-M100%",
                "--rename-empty",
                "--name-status",
                "-z",
                "--format=%x01%H%x00%cI%x00%ae",
                "--",
            ],
            Some(lines(ids)),
            OUTPUT_LIMIT,
        )
        .await?;
        parse_log(&output)
    }

    /// The promisor remote of a clone made with a `tree:` filter.
    async fn tree_promisor(&self) -> Option<String> {
        let output = run(
            &self.root,
            &[
                "config",
                "--get-regexp",
                r"^remote\..+\.partialclonefilter$",
                r"^tree:[0-9]+$",
            ],
            None,
            8192,
        )
        .await
        .ok()?;
        let line = std::str::from_utf8(&output).ok()?.lines().next()?;
        let (key, _) = line.rsplit_once(' ')?;
        let remote = key
            .strip_prefix("remote.")?
            .strip_suffix(".partialclonefilter")?;
        (!remote.is_empty() && !remote.starts_with('-')).then(|| remote.to_owned())
    }

    /// Fetches the missing root trees of the commits and their parents, `TREE_FETCH_BATCH` per
    /// request. `blob:none` brings each tree with its subtrees, so `git log` then reads locally
    /// instead of fetching one tree at a time. Finished batches stay even if a later one fails.
    async fn prefetch_trees(&self, remote: &str, ids: &[String]) -> Result<()> {
        let listed = run(
            &self.root,
            &[
                "log",
                "--stdin",
                "--no-walk=unsorted",
                "--no-show-signature",
                "--format=%T %P",
            ],
            Some(lines(ids)),
            OUTPUT_LIMIT,
        )
        .await?;
        let mut trees = BTreeSet::new();
        let mut parents = BTreeSet::new();
        for line in std::str::from_utf8(&listed)?.lines() {
            let mut fields = line.split(' ').filter(|field| !field.is_empty());
            trees.insert(fields.next().context("Missing Git tree ID")?.to_owned());
            parents.extend(fields.map(str::to_owned));
        }
        for id in ids {
            parents.remove(id);
        }
        if !parents.is_empty() {
            let listed = run(
                &self.root,
                &[
                    "log",
                    "--stdin",
                    "--no-walk=unsorted",
                    "--no-show-signature",
                    "--format=%T",
                ],
                Some(lines(&parents)),
                OUTPUT_LIMIT,
            )
            .await?;
            trees.extend(std::str::from_utf8(&listed)?.lines().map(str::to_owned));
        }
        ensure!(trees.iter().all(|id| valid_sha(id)), "Invalid Git tree ID");
        let checked = run(
            &self.root,
            &["cat-file", "--batch-check"],
            Some(lines(&trees)),
            OUTPUT_LIMIT,
        )
        .await;
        let missing: Vec<&str> = match &checked {
            Ok(output) => std::str::from_utf8(output)?
                .lines()
                .filter_map(|line| line.strip_suffix(" missing"))
                .collect(),
            // Older Git can abort the local probe when an object would require a fetch.
            // Send the candidate trees through the same bounded, explicit fetch in that case.
            Err(_) => trees.iter().map(String::as_str).collect(),
        };
        for batch in missing.chunks(TREE_FETCH_BATCH) {
            let mut fetch = command(
                &self.root,
                &[
                    "-c",
                    "fetch.negotiationAlgorithm=noop",
                    "fetch",
                    "--quiet",
                    "--no-tags",
                    "--no-write-fetch-head",
                    "--recurse-submodules=no",
                    "--no-auto-maintenance",
                    "--filter=blob:none",
                    "--stdin",
                    "--",
                    remote,
                ],
                true,
            );
            allow_promisor_fetch(&mut fetch);
            run_command(fetch, Some(lines(batch)), 8192, FETCH_TIMEOUT).await?;
        }
        Ok(())
    }

    pub(super) async fn snapshot(&self, sha: &str) -> Result<Vec<SourceFile>> {
        let listing = run(
            &self.root,
            &["ls-tree", "-r", "-z", "--full-tree", sha],
            None,
            OUTPUT_LIMIT,
        )
        .await?;
        let paths = parse_tree(&listing)?;
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let requests = lines(paths.iter().map(|&(_, oid)| oid));
        let bytes = run(
            &self.root,
            &["cat-file", "--batch"],
            Some(requests),
            OUTPUT_LIMIT,
        )
        .await?;
        parse_blobs(&bytes, &paths)
    }

    /// Current branch (`None` on a detached HEAD) and the working-tree changes, following the
    /// IDX-3-02 working-file contract: at most 2,000 distinct paths. Paths outside the central
    /// path rules are skipped.
    /// The current branch, the changed files central can take, and how many changed files were
    /// left out to stay within its limit.
    pub(crate) async fn working(
        &self,
    ) -> Result<(Option<String>, Vec<(RelativePath, WorkingChange)>, usize)> {
        let branch = match run(&self.root, &["branch", "--show-current"], None, 1024).await {
            Ok(raw) => std::str::from_utf8(&raw)
                .ok()
                .map(|name| name.trim_end_matches(['\r', '\n']))
                .filter(|name| !name.is_empty())
                .map(str::to_owned),
            Err(error) if error.is::<OutputLimitExceeded>() => None,
            Err(error) => return Err(error),
        };
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
        let (files, omitted) = within_central_limit(parse_status(&status)?);
        Ok((branch, files, omitted))
    }
}

fn parse_tree(listing: &[u8]) -> Result<Vec<(&str, &str)>> {
    ensure!(
        listing.is_empty() || listing.ends_with(b"\0"),
        "Truncated Git tree entry"
    );
    let mut paths = Vec::new();
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
        ensure!(valid_sha(oid), "Invalid Git blob ID");
        paths.push((path, oid));
    }
    Ok(paths)
}

fn parse_blobs(bytes: &[u8], paths: &[(&str, &str)]) -> Result<Vec<SourceFile>> {
    let mut position = 0;
    let mut files = Vec::with_capacity(paths.len());
    for &(path, oid) in paths {
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

fn valid_sha(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `--format=%x01%H%x00%cI%x00%ae --name-status -z`: each commit is `\x01SHA\0TIME\0EMAIL\0`
/// (the email may be empty), then `\n` and NUL-separated status/path entries where renames and
/// copies carry the old and the new path.
fn parse_log(output: &[u8]) -> Result<Vec<RawCommit>> {
    if output.is_empty() {
        return Ok(Vec::new());
    }
    let output = output.strip_suffix(b"\0").context("Unterminated Git log")?;
    let mut fields = output.split(|b| *b == 0).peekable();
    let mut commits = Vec::new();
    while let Some(header) = fields.next() {
        let sha = header
            .strip_prefix(b"\x01")
            .context("Invalid Git log header")?;
        let time = fields.next().context("Missing Git commit time")?;
        let email = fields.next().context("Missing Git commit author")?;
        let sha = std::str::from_utf8(sha).context("Invalid Git commit ID")?;
        ensure!(valid_sha(sha), "Invalid Git commit ID");
        let mut files = Vec::new();
        let mut status = fields
            .next_if(|field| !field.starts_with(b"\x01"))
            .map(|field| field.strip_prefix(b"\n").unwrap_or(field));
        while let Some(raw_status) = status {
            let change = std::str::from_utf8(raw_status).context("Invalid Git status")?;
            if matches!(raw_status.first(), Some(b'R' | b'C')) {
                fields.next().context("Rename without source path")?;
            }
            let path = fields.next().context("Status without path")?;
            // Central cannot take a non-UTF-8 path; skip that file only.
            if let Ok(path) = std::str::from_utf8(path) {
                files.push((change.to_owned(), path.to_owned()));
            }
            status = fields.next_if(|field| !field.starts_with(b"\x01"));
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
        let Ok(path) = RelativePath::for_working(path) else {
            continue;
        };
        files.insert(path, WorkingChange::from_porcelain([record[0], record[1]]));
    }
    Ok(files.into_iter().collect())
}

/// Central takes at most `MAX_WORKING_FILES` changed files. Past that, changes to tracked files
/// are kept first and untracked files fill the rest, so files a missing `.gitignore` entry
/// exposes (such as `node_modules`) cannot push the real work out. Each group keeps path order,
/// so the same tree always gives the same list. Returns the kept files in path order and how
/// many were left out.
fn within_central_limit(
    files: Vec<(RelativePath, WorkingChange)>,
) -> (Vec<(RelativePath, WorkingChange)>, usize) {
    let limit = crate::git_scope::MAX_WORKING_FILES;
    if files.len() <= limit {
        return (files, 0);
    }
    let omitted = files.len() - limit;
    let (tracked, untracked): (Vec<_>, Vec<_>) = files
        .into_iter()
        .partition(|(_, change)| *change != WorkingChange::Untracked);
    let mut kept: Vec<_> = tracked.into_iter().chain(untracked).take(limit).collect();
    kept.sort_by(|a, b| a.0.cmp(&b.0));
    (kept, omitted)
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

fn command(root: &Path, args: &[&str], input: bool) -> Command {
    use std::process::Stdio;
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        // Git before 2.45 ignores GIT_NO_LAZY_FETCH. An empty allowlist still blocks
        // transports for local reads, including any fetch Git starts for a missing object.
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("GCM_INTERACTIVE", "never")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .stdin(if input { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // 백그라운드 브로커는 콘솔이 없어서, 플래그 없이 git을 띄우면 3초마다 콘솔 창이 깜박인다.
    #[cfg(windows)]
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    command
}

async fn history_ids(root: &Path, range: &str, after: u64) -> Result<Vec<String>> {
    timeout(GIT_TIMEOUT, async {
        let mut child = command(
            root,
            &[
                "rev-list",
                "--timestamp",
                "--no-merges",
                "--topo-order",
                "--reverse",
                range,
                "--",
            ],
            false,
        )
        .spawn()
        .context("Cannot start Git")?;
        let stdout = child.stdout.take().context("Cannot read Git output")?;
        let mut reader = BufReader::new(stdout);
        let mut line = Vec::new();
        let mut ids = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line).await? == 0 {
                break;
            }
            ensure!(line.len() <= 128, "Invalid Git history entry");
            let text = std::str::from_utf8(&line)?.trim_end_matches(['\r', '\n']);
            let (timestamp, sha) = text.split_once(' ').context("Invalid Git history entry")?;
            ensure!(valid_sha(sha), "Invalid Git commit ID");
            let timestamp: i64 = timestamp.parse().context("Invalid Git commit time")?;
            if u64::try_from(timestamp).is_ok_and(|time| time >= after) {
                ids.push(sha.to_owned());
            }
        }
        ensure!(child.wait().await?.success(), "Git operation failed");
        Ok(ids)
    })
    .await
    .context("Git operation timed out")?
}

/// Restores the caller's transport policy for the explicit promisor fetch.
fn allow_promisor_fetch(command: &mut Command) {
    if let Some(protocols) = std::env::var_os("GIT_ALLOW_PROTOCOL") {
        command.env("GIT_ALLOW_PROTOCOL", protocols);
    } else {
        command.env_remove("GIT_ALLOW_PROTOCOL");
    }
    command.env("GIT_NO_LAZY_FETCH", "0");
}

/// One ID per line for `--stdin` and `cat-file --batch-check`.
fn lines<S: AsRef<str>>(ids: impl IntoIterator<Item = S>) -> Vec<u8> {
    let mut bytes = Vec::new();
    for id in ids {
        bytes.extend_from_slice(id.as_ref().as_bytes());
        bytes.push(b'\n');
    }
    bytes
}

async fn run(root: &Path, args: &[&str], input: Option<Vec<u8>>, limit: usize) -> Result<Vec<u8>> {
    run_command(
        command(root, args, input.is_some()),
        input,
        limit,
        GIT_TIMEOUT,
    )
    .await
}

async fn run_command(
    mut command: Command,
    input: Option<Vec<u8>>,
    limit: usize,
    wait: Duration,
) -> Result<Vec<u8>> {
    timeout(wait, async {
        let mut child = command.spawn().context("Cannot start Git")?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().context("Cannot read Git output")?;
        let mut bytes = Vec::new();
        let mut reader = stdout.take((limit + 1) as u64);
        tokio::try_join!(
            async {
                reader.read_to_end(&mut bytes).await?;
                if bytes.len() > limit {
                    return Err(OutputLimitExceeded.into());
                }
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
    fn tree_entries_keep_utf8_source_paths_and_skip_links_and_submodules() {
        let oid = "1".repeat(40);
        let listing = format!(
            "100644 blob {oid}\tsrc/한 글.ts\0\
             100755 blob {oid}\tbuild.js\0\
             120000 blob {oid}\tlink.ts\0\
             160000 commit {oid}\tmodule.ts\0\
             100644 blob {oid}\t../outside.ts\0\
             100644 blob {oid}\tREADME.md\0"
        );
        assert_eq!(
            parse_tree(listing.as_bytes()).unwrap(),
            [("src/한 글.ts", oid.as_str()), ("build.js", oid.as_str())]
        );
        assert!(parse_tree(listing.trim_end_matches('\0').as_bytes()).is_err());
        assert!(parse_tree(b"100644 blob invalid\tfile.ts\0").is_err());
        assert!(parse_tree(b"no tab\0").is_err());
    }

    #[test]
    fn batch_blob_records_use_the_declared_byte_length_and_expected_object_id() {
        let first = "1".repeat(40);
        let second = "2".repeat(40);
        let paths = [("a.ts", first.as_str()), ("b.ts", second.as_str())];
        let body = format!("{first} blob 5\n한\nx\n{second} blob 0\n\n");
        let files = parse_blobs(body.as_bytes(), &paths).unwrap();
        assert_eq!(
            (files[0].path.as_str(), files[0].text.as_str()),
            ("a.ts", "한\nx")
        );
        assert_eq!(
            (files[1].path.as_str(), files[1].text.as_str()),
            ("b.ts", "")
        );
        assert!(parse_blobs(body.trim_end_matches('\n').as_bytes(), &paths).is_err());
        assert!(parse_blobs(format!("{body}\n").as_bytes(), &paths).is_err());
        assert!(parse_blobs(body.as_bytes(), &[paths[1], paths[0]]).is_err());
        assert!(parse_blobs(format!("{first} missing\n").as_bytes(), &paths).is_err());
    }

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
    fn status_skips_paths_central_rejects() {
        let files = parse_status(b" M ok.ts\0 M bad\\name.ts\0?? \xff\xfe\0").unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0.as_str(), "ok.ts");
        assert!(parse_status(b"XM\0").is_err());
        assert!(parse_status(b"MMXpath\0").is_err());
    }

    #[test]
    fn past_the_central_limit_tracked_changes_come_first_and_untracked_files_fill_the_rest() {
        let limit = crate::git_scope::MAX_WORKING_FILES;
        let within: Vec<u8> = (0..limit)
            .flat_map(|i| format!("?? f{i:05}\0").into_bytes())
            .collect();
        let (files, omitted) = within_central_limit(parse_status(&within).unwrap());
        assert_eq!((files.len(), omitted), (limit, 0));

        // 2,500 untracked files sort before the three real changes, which must still survive.
        let mut status: Vec<u8> = (0..2_500)
            .flat_map(|i| format!("?? a/node_modules/m{i:05}.js\0").into_bytes())
            .collect();
        status.extend_from_slice(b" M src/z.ts\0D  src/gone.ts\0UU src/both.ts\0");
        let (files, omitted) = within_central_limit(parse_status(&status).unwrap());
        assert_eq!((files.len(), omitted), (limit, 503));
        let tracked: Vec<_> = files
            .iter()
            .filter(|(_, change)| *change != WorkingChange::Untracked)
            .map(|(path, change)| (path.as_str(), *change))
            .collect();
        assert_eq!(
            tracked,
            [
                ("src/both.ts", WorkingChange::Unmerged),
                ("src/gone.ts", WorkingChange::Deleted),
                ("src/z.ts", WorkingChange::Modified),
            ]
        );
        // Untracked files fill the rest in path order, and the list stays sorted by path.
        assert_eq!(files[0].0.as_str(), "a/node_modules/m00000.js");
        assert_eq!(files[limit - 4].0.as_str(), "a/node_modules/m01996.js");
        assert!(files.windows(2).all(|pair| pair[0].0 < pair[1].0));
        // The same tree always gives the same list.
        assert_eq!(
            within_central_limit(parse_status(&status).unwrap()),
            (files, omitted)
        );
    }

    #[test]
    fn status_discards_overlong_utf8_paths_before_counting_working_files() {
        let mut status: Vec<u8> = (0..crate::git_scope::MAX_WORKING_FILES)
            .flat_map(|i| format!("?? f{i}\0").into_bytes())
            .collect();
        let rejected = format!("{}/{}", "가".repeat(167), "나".repeat(167));
        status.extend_from_slice(format!("?? {rejected}\0").as_bytes());
        assert_eq!(
            parse_status(&status).unwrap().len(),
            crate::git_scope::MAX_WORKING_FILES
        );
        let boundary = format!("{}/{}", "가".repeat(166), "나".repeat(167));
        let files = parse_status(format!("?? {boundary}\0").as_bytes()).unwrap();
        assert_eq!(files[0].0.as_str(), boundary);
    }

    #[test]
    fn status_counts_working_paths_in_code_points_and_bytes() {
        let astral = [50, 50, 50, 50, 49]
            .map(|count| "😀".repeat(count))
            .join("/");
        let too_many_points = ["x".repeat(250), "y".repeat(250)].join("/");
        assert_eq!(astral.len(), 1000);
        assert!(RelativePath::new(&astral).is_err());
        let status = format!("?? {astral}\0?? {too_many_points}\0");
        let files = parse_status(status.as_bytes()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0.as_str(), astral);
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

    #[tokio::test]
    async fn real_git_objects_preserve_control_bytes_in_paths_and_empty_commits() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run(root, &["init", "-q", "--bare"], None, 4096)
            .await
            .unwrap();
        let blob = run(
            root,
            &["hash-object", "-w", "--stdin"],
            Some(b"text".to_vec()),
            128,
        )
        .await
        .unwrap();
        let blob = std::str::from_utf8(&blob).unwrap().trim();
        let entries = ["a\x01b.txt", "\nlead.txt", "normal.txt"]
            .map(|path| format!("100644 blob {blob}\t{path}\0"))
            .concat();
        let tree = run(root, &["mktree", "-z"], Some(entries.into_bytes()), 128)
            .await
            .unwrap();
        let tree = std::str::from_utf8(&tree).unwrap().trim();
        let mut ids: Vec<String> = Vec::new();
        for _ in 0..2 {
            let mut args = vec![
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.test",
                "commit-tree",
                tree,
                "-m",
                "test",
            ];
            if let Some(parent) = ids.last() {
                args.extend(["-p", parent.as_str()]);
            }
            let sha = run(root, &args, None, 128).await.unwrap();
            ids.push(std::str::from_utf8(&sha).unwrap().trim().to_owned());
        }
        let git = GitProject {
            root: root.to_owned(),
            remote: "origin".into(),
            key: String::new(),
        };
        let commits = git.commits(&ids).await.unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(
            commits[0].files,
            [
                ("A".to_owned(), "\nlead.txt".to_owned()),
                ("A".to_owned(), "a\x01b.txt".to_owned()),
                ("A".to_owned(), "normal.txt".to_owned()),
            ]
        );
        assert!(commits[1].files.is_empty());
        let allowed = commits[0]
            .files
            .iter()
            .filter(|(_, path)| RelativePath::new(path).is_ok())
            .collect::<Vec<_>>();
        assert_eq!(allowed.len(), 1);
        assert_eq!(allowed[0].1, "normal.txt");
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
