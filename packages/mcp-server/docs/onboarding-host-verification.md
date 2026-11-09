# Actual Host onboarding verification (S15P31D108-144)

This opt-in harness exercises Codex and Claude Code against the built MCP server,
native Rust broker, and an isolated loopback Spring/PostgreSQL server. It does not
connect to EC2, deploy, alter the user's global Host configuration, or mark Jira
stories complete. Existing first-MVP work/result roundtrip checks remain separate.

## Preparation

- Node 24+, Python 3.11+ (`tomllib`), a built Rust CLI and `mcp-config` example.
- Codex 0.166.0 and Claude Code 2.1.317 with usable existing logins. Claude uses
  exactly `claude-opus-5-5` / `high`; Codex preserves the user's configured model.
- A dedicated IPv4 loopback central server with disposable DB data. Record its
  exact 40-character source commit. Production URLs are rejected.
- Use an absolute artifact directory outside tracked source. Captured transcripts
  are scanned for known synthetic credentials and copied Codex login tokens.

```powershell
npm run build --prefix packages/mcp-server
cargo +stable-x86_64-pc-windows-msvc build --manifest-path rust/Cargo.toml -p devlink-cli --locked
cargo +stable-x86_64-pc-windows-msvc build --manifest-path rust/Cargo.toml -p devlink-broker --example mcp-config --locked
$env:DEVLINK_TEST_CENTRAL_URL='http://127.0.0.1:61907/'
$env:DEVLINK_TEST_CENTRAL_COMMIT='<exact isolated server source SHA>'
$env:DEVLINK_TEST_CLI=(Resolve-Path rust/target/debug/devlink.exe).Path
$env:DEVLINK_TEST_CONFIG_PROVIDER=(Resolve-Path rust/target/debug/examples/mcp-config.exe).Path
$env:DEVLINK_TEST_MCP_ENTRY=(Resolve-Path packages/mcp-server/dist/index.js).Path
$env:DEVLINK_TEST_HOST_ARTIFACTS=Join-Path $PWD '.planning/onboarding-host-artifacts'
node packages/mcp-server/scripts/verify-onboarding-hosts.mjs
```

This process-cleanup harness currently targets Windows. Unix module tests and
Host verification must be recorded separately. This harness creates two
synthetic accounts, one team and one Agent per account only on that disposable
central server. It counts Agent-create and token-issue HTTP requests without
recording authorization headers, cookies or request bodies. The private local
profiles and brokers are cleaned up; synthetic central records remain in the
isolated DB for inspection.

## Native configuration and trust

The Rust example creates each project's `.codex/config.toml` or `.mcp.json` and
checks it with `McpConfig.inspect`. Absolute resolved Host executable paths are
hashed and used for launch. Codex uses a read-only sandbox and never prompts;
Claude has no built-in tools. Each entry references a unique bootstrap
script that imports the actual built MCP runtime. Native tool calls, not a mock
MCP service, must prove that the generated configuration is consumed.

Codex loads project configuration only for trusted projects
([official configuration guide](https://learn.chatgpt.com/docs/config-file/config-basic)).
For these temporary repositories, the harness uses a private temporary
`CODEX_HOME` with an access-token-only login and model/provider/Windows preferences, plus
persisted trust for that one project. The access token must be valid for at least one more hour and the existing login
must have been refreshed in the past 24 hours. Its refresh token is cleared before
writing the private copy, so the child cannot consume/rotate the original refresh
token. An authentication-file change fails the run. The private directory is
deleted after successful cleanup. If a broker cannot be stopped, the private
fixture is preserved and `retainedFixturePath` identifies it for recovery. It
contains access credentials: stop the owned broker before removing it. It never modifies the
original login or trust file. The actual private Codex native inventory must contain exactly DevLink, with the
expected command, arguments and credential path. Claude receives the generated project file with `--strict-mcp-config`, empty
built-in tools, restricted mode and hooks disabled. Its existing login and normal
CLI bookkeeping still use the original Claude location; the harness does not
edit those files itself. Managed policies still apply.
A configuration parser or `inspect=applied` result alone does not prove LLM calls.

## Assertions and measurements

For each Host, actual correlated tool events must show this exact sequence with
empty argument objects:

1. `get_my_context` -> verified expected user/Agent/tool.
2. `get_connection_status` -> connected, verified, valid credentials, unbound.
3. `register_agent({})` -> original Agent ID.
4. `get_connection_status` -> bound.
5. `get_my_context` -> same principal, bound.
6. `register_agent({})` -> same Agent ID again.

The flow is repeated after Host restart and broker restart. A prior stdio process
may exit before DELETE arrives: an immediate restart must respect the existing
owner, then wait for the 90-second lease to expire before opening a fresh Host.
Read-only probes do not claim or renew the lease. The report records the observed
wait rather than claiming immediate recovery.

A separately held SDK session proves active-owner rejection; that holder is
closed explicitly. Further Host calls diagnose a stopped broker and missing
credential file, then validate normal recovery. The report must retain exact
native results and transcript hashes; prose saying "success" cannot satisfy it.
Wrong subject, additional metadata, missing/extra calls or an unbound result fail
validation. The configuration and stored central token must stay unchanged, and
additional central Agent-create/token-issue requests must both be zero.
The preparation counters must first prove 2 creates and 2 token issues (4 Agent
mutations); all later non-read Agent requests are counted by URL pathname. Stable
Agent metadata snapshots and the holder's retained binding provide separate checks.

Report configuration-apply and Host-step elapsed times, lease wait, setup and
post-setup request counts, and additional metadata inputs. These are observations
from one run. A manual baseline time has not been measured, so this does not claim
a percentage speedup, success rate or zero total onboarding time.

## Evidence levels

| Condition | Required evidence |
| --- | --- |
| Generated config, verified context, registration, repeat/restart | Native Host + real Rust + real isolated central |
| Active owner, prior lease, stopped broker, missing credentials | Native Host results and explicit recovery guidance |
| Central disconnect and rejected Agent authentication | `connection.test.mjs`, native Rust `connection_api/context_api` regressions; separate from a real Host outage |
| Changed user/team/Agent/project | MCP `context.test.mjs` and native Rust context/binding regressions |
| CLI login expiry and uncertain creation/pending | Native CLI `setup_authentication` / `cli_agents` tests against controlled servers |
| Existing task authorization, explicit acceptance and result roundtrip | First-MVP regression and `verify-central-roundtrip.mjs`; separate Host task harness |

Unavailable conditions stay explicitly unverified at the corresponding level.
The report does not turn fixture results into real-Host evidence or change Jira
completion state. Before commit, review the harness/evidence parser with the
required exact Opus model and keep its actual model ID with the run evidence.

The report records pre-commit source dirtiness and a startup hash of the verifier
code, plus full MCP build and executable hashes. `clientSourceCommit` is the
baseline when the verifier is uncommitted. Effort is the requested CLI option;
Claude modelUsage verifies the model, while native Codex events do not expose the
resolved model and this remains explicitly unverified.
