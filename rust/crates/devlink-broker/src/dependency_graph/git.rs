use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::timeout,
};

const OUTPUT_LIMIT: usize = 16 * 1024 * 1024;

pub(super) struct GitProject {
    root: PathBuf,
    remote: String,
    pub key: String,
}

pub(super) struct SourceFile {
    pub path: String,
    pub text: String,
}

impl GitProject {
    pub async fn open(path: &Path, remote: &str) -> Result<Self> {
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

    pub async fn sha(&self, branch: &str) -> Result<String> {
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

    pub async fn require_ancestor(&self, old: &str, new: &str) -> Result<()> {
        run(
            &self.root,
            &["merge-base", "--is-ancestor", old, new],
            None,
            128,
        )
        .await?;
        Ok(())
    }

    pub async fn snapshot(&self, sha: &str) -> Result<Vec<SourceFile>> {
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
    /// IDX-3-02 working-file contract: at most 2,000 distinct paths.
    pub async fn working(&self) -> Result<(Option<String>, BTreeMap<String, &'static str>)> {
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
        let mut files = BTreeMap::new();
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
            if !valid_path(path) {
                continue;
            }
            let flags = &record[..2];
            let change = match flags {
                b"??" => "UNTRACKED",
                b"DD" | b"AU" | b"UD" | b"UA" | b"DU" | b"AA" | b"UU" => "UNMERGED",
                _ if flags.contains(&b'D') => "DELETED",
                _ if flags.contains(&b'A') => "ADDED",
                _ if flags.contains(&b'T') => "TYPE_CHANGED",
                _ => "MODIFIED",
            };
            files.insert(path.to_owned(), change);
            ensure!(files.len() <= 2000, "Too many changed files");
        }
        Ok((branch, files))
    }
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
        let mut child = Command::new("git")
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
            .kill_on_drop(true)
            .spawn()
            .context("Cannot start Git")?;
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
