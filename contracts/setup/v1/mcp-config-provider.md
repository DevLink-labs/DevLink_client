# Rust McpConfig provider (S15P31D108-143)

`devlink_broker::mcp_config` implements setup/v1 section 6.2. This is an in-process
provider. 최준혁's -111 owns calling it from the product setup CLI; -108 owns Host
discovery and runtime verification. The module does not install or start a Host,
log in, select a team, create an Agent, issue a token, or deploy anything.

Create `VerifiedHost::new(toolType, detectedVersion)` and then `McpConfig::new(host)`.
The tested automatic formats are **Codex 0.166.0** and **Claude Code 2.1.310**.
Unknown versions return `SETUP_TOOL_UNAVAILABLE` for a manual path. Supporting a
new version requires its actual parser/Host verification, not silently guessing.

The -108 adapter must verify Node >=24 and the built MCP entry before calling the
provider. Version 1 accepts only an existing absolute Node executable and argv
`[absoluteScript.js-or-mjs, "--transport", "stdio"]`. It rejects arbitrary shell
strings, other launchers and token-bearing argv. Configuration env contains only
`DEVLINK_BROKER_FILE`, an absolute private descriptor path; no token values.
The adapter-supplied stable Node/script paths are written to the Host config;
their resolved files, stable alias ownership and parent write permissions are
checked again at apply. Unix
system-owned Node binaries and system-owned Windows installations are permitted.
The provider references an existing credential descriptor; it never creates,
copies or moves it. Credential placement and VCS exclusion remain the bootstrap
caller's responsibility, including existing project `.devlink-runtime` layouts.
Paths are literal: control characters and `${...}` are rejected so Host variable
expansion cannot redirect the selected executable, script or credential file.

## API

Setup integration also exposes `mcp_config::preflight(input, inspect_target)`.
It validates the same paths/permissions and, when `inspect_target` is true, the
existing Host entry through the provider's merge parser. It rejects conflicts
without writing or retaining an apply plan. The future descriptor may be absent;
an existing descriptor must still be safe and valid. Setup calls this before
Agent/token creation, probes the runtime separately, and repeats validation in
the existing plan/apply path. Manual mode uses `inspect_target=false` because it
only returns guidance rather than merging into the current Host file.

```rust,ignore
use devlink_broker::mcp_config::{McpConfig, VerifiedHost, PlanInput, InspectionInput};
let mut provider = McpConfig::new(VerifiedHost::new(tool_type, detected_version)?);
let plan = provider.plan(input)?;
// The CLI presents plan.diff, then applies this same in-memory plan/digest.
let applied = provider.apply(&plan.plan_id, plan.expected_digest.as_deref())?;
let inspected = provider.inspect(inspection_input)?;
```

- `PlanInput`: `toolType/projectPath/targetPath/brokerFile/executable{command,args}`.
- `InspectionInput`: `toolType/targetPath/brokerFile/executable` (project inferred
  from the fixed project filename). Tool type must match the verified provider.
- `ConfigPlan`: `planId/toolType/targetPath/expectedDigest/change/diff/restartRequired`.
  `change` is create/merge/unchanged/conflict; missing target has null digest.
  `diff` contains only the proposed DevLink item and its change classification.
  It never includes original configuration, unrelated env values or backup bytes.
- `apply`: applied/unchanged, targetPath, optional backupPath, restartRequired.
- `inspect`: applied/missing/conflict/unknown and targetPath.
- Errors have fixed code/outcome/rollback and optional safe backupPath; parser,
  OS and subprocess messages are not copied. A full backup store returns the safe
  `recoveryDirectory` for manual review with WRITE_FAILED/not_applied (not a backup file). Rollback is restored/not_needed/failed/conflict.

Keep the provider instance between plan and apply. Plans are not persisted and
retain the 32 most recent IDs per instance; older plans expire and must be replanned. An unknown ID or wrong digest is a conflict.
Methods perform bounded synchronous file/ACL I/O; an async CLI should call them
on its blocking executor and keep the result/outcome when cancelling orchestration.
Windows ACL validation invokes bounded subprocesses and can take seconds per
operation; it must not run on the CLI's asynchronous event loop.

## Files and concurrency

Only `<canonical project>/.codex/config.toml` or `<canonical project>/.mcp.json`
is writable. Home/global targets, path traversal, linked targets/parents, wrong
ownership and unsafe write permissions are rejected. Existing Host files may be readable by others (Unix 0644), but other-user writes
and group-writable projects (including Unix 0775/0664) require a manual safe path.
The module does not chmod existing projects. New config/lock/backup files are
private. Merge and restore preserve the existing Host file mode/group/read ACL; backup
files stay private regardless of the original Host permissions. Locks and backups live outside projects under the current user's
home `.devlink-mcp-config/<target-sha256>/` directory, shared across
profiles targeting the same file. A target retains up to 64 backups; a full store
fails before changing Host content and requires manual recovery-file review.
Backup paths are returned only for files actually created. No backup is made for
a new target or an unchanged repeat. Both input and proposed Host configuration are bounded to 1 MiB;
the credential descriptor keeps its separate 16 KiB limit.

Plan and inspect write no files. Apply validates the current descriptor before
writing. It takes a per-target advisory lock, rechecks the expected digest, saves
and flushes an exact private backup, prepares and flushes a private temporary file,
rechecks the digest immediately before atomic replacement, then reparses the entry.
Create uses no-clobber replacement. Other MCP items/settings are preserved; TOML
comments/order are retained where toml_edit supports it, JSON data is preserved
with original root/server key order and exact unrelated numeric values. The root
is indented; unrelated values keep their original text, while the merged `mcpServers` object
and new entry are written compact. Whitespace is not byte-for-byte preserved. Duplicate JSON keys, incompatible container shapes, or
any different existing DevLink entry are conflicts, without a silent overwrite.

If inspection fails, restore an existing file only over the exact version this
apply wrote. For a newly created file, portable compare-and-delete cannot exclude
an arbitrary editor race: retain it and return `outcome=unknown`,
`rollback=not_needed`, with no backup. Inspect it before retrying. Restoration
also rechecks its expected digest after temporary file preparation. An observed
concurrent edit wins; retain the backup and return unknown/conflict. Failed restore
returns unknown/failed. Advisory locking coordinates provider callers, while the
digest fences detect external edits at the observation points; this is not a
filesystem transaction with arbitrary editors. Preserve backup/outcome for manual
inspection if the external state cannot be verified.

An unchanged repeat does not rewrite the Host file or create another backup.
File `applied` is distinct from a running Host using it. Project trust, restarting
the Host and actual MCP tool calls remain separate verification steps. The broker's
minimal ConnectionSnapshot cannot infer mcpConfig=applied from being called.

## Provider example

`cargo build --manifest-path rust/Cargo.toml -p devlink-broker --example mcp-config --locked`
builds a small demonstration harness. It accepts the example JSON on stdin and
`--plan` (default), `--inspect`, or `--apply`; each invocation owns a new provider.
`--apply` plans and applies within that one invocation. This harness is for
reproduction/integration tests, not a replacement for -111's setup flow or a
persistent reviewed-plan store. The product CLI must retain the same plan instance.

```json
{
  "hostVersion": "2.1.310",
  "input": {
    "toolType": "claude_code",
    "projectPath": "C:\\work\\demo",
    "targetPath": "C:\\work\\demo\\.mcp.json",
    "brokerFile": "C:\\work\\profile\\.devlink-runtime\\broker.json",
    "executable": {
      "command": "C:\\Program Files\\nodejs\\node.exe",
      "args": ["C:\\work\\DevLink\\packages\\mcp-server\\dist\\index.js", "--transport", "stdio"]
    }
  }
}
```

Format references: [official Codex MCP configuration](https://learn.chatgpt.com/docs/extend/mcp#connect-codex-to-an-mcp-server),
[official Claude project MCP configuration](https://code.claude.com/docs/en/mcp#project-scope),
[toml_edit 0.25 documentation](https://docs.rs/toml_edit/0.25.17+spec-1.1.0/toml_edit/).
