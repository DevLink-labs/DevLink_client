# MCP onboarding (S15P31D108-140)

`get_my_context({})` reads the existing Rust broker through `GET /v1/context`.
It does not claim, bind, renew a lease, publish presence, or change work.
The endpoint accepts only MCP `localToken` Bearer authentication; any Origin is rejected.
GET has no body or query. `X-Request-Id` is correlation and `X-Instance-Id` selects
this MCP process's existing session; neither supplies a business principal.

The verified principal comes from live central `/api/v1/agents/me`, fenced against
the current WebSocket generation/identity. IDs are positive canonical decimal strings
up to Java Long. Missing metadata is null, never caller/config defaults. Agent metadata
is labelled `central_agents_me`. Local project paths are labelled `bound_profile`
only after the stored profile, selected team, current origin, existing canonical
project directory and live principal agree; otherwise no path is inferred from cwd.
An environment-token broker without a bound profile reports project `unavailable`.

Example after CLI preparation, before explicit registration:

```json
{"requestId":"context-1","data":{"identity":"verified","principal":{"userId":"3","teamId":"1","agentId":"12","toolType":"codex"},"agent":{"name":"worker","roleDescription":null,"toolType":"codex","projectName":"example","source":"central_agents_me"},"project":{"path":null,"source":"unavailable"},"hostSession":"unbound"}}
```

Disconnected reads contain `identity: unavailable`, null principal/Agent/project path
and `hostSession: not_checked`. Authentication rejection and mismatched identities
remain errors. A changed profile/project/origin must be explicitly inspected and
rebound through the CLI; queries do not switch it. MCP rejects changed observed
user/team/Agent/tool/project context and requires a process restart after reconfiguration.

`context.schema.json` in `contracts/local-broker/v1` is generated from the same
runtime Zod schema; cross-field consistency and the Java Long upper bound are additionally checked at runtime.
The legacy `principal.schema.json` remains compatible with opaque v0.1 session IDs.
Query errors do not discard a lease or stop its heartbeat. A verified contradiction
blocks subsequent business calls while preserving the existing session lifecycle.

## -140 verification (2026-11-02, Asia/Seoul)

Baseline: merged client develop 64e3155, server develop 74654df.
MCP full regression: 125 passed; subsequent focused context suite: 11 passed.
Rust broker library: 63 passed in serial mode. A preexisting storage timing test
failed during the initial parallel run and passed in the complete serial rerun.
Actual SDK stdio / native Rust / isolated Spring + PostgreSQL: live principal,
project provenance, unbound-before/bound-after registration and both task directions passed.
This evidence does not claim a new real LLM Host execution (-144).

Claude Code CLI cross-review used actual model `claude-opus-5-5`, effort `high`,
in two rounds. Query error/heartbeat isolation, AuthFailed distinction, generation
fence and unbound-owner conflict findings were fixed. Round 2 reported no must-fix
or should-fix defects. Additional focused tests cover bound-context contradiction
without heartbeat loss and real 503 domain/transport error classification.
Evidence: `.planning/mvp2-implementation-2026-11-02/` (private, ignored).

## Connection diagnostics (S15P31D108-141)

`get_connection_status({})` consumes authenticated, bodyless `GET /v1/connection`
with the same Origin/header rules as context. MCP process health is separate from
credential validity, broker reachability, central WebSocket state, live identity,
and Host ownership. `nextActions` are fixed advice identifiers; no commands run.

The fields follow `contracts/setup/v1` ConnectionSnapshot. This minimal supplier
cannot inspect the Host config the caller loaded, so `mcpConfig=unknown` and
`setupReady=false` even if the Host session is bound. CLI Connection.inspect can
combine the separate McpConfig.inspect evidence. `collaborationReady` follows the
contract formula, and does not attest to a completed task roundtrip.

Missing credentials, invalid/private-file or local auth failures, unreachable
broker, central disconnect/auth denial, conflict and protocol failures remain
separate. An unreachable HTTP port does not prove that an owned OS broker is
stopped, so MCP reports `broker=unknown` and `inspect_local_broker`. Rust verifies
the current private credential snapshot matches its own URL/token before `valid`.
`unbound` means no current valid lease; it does not infer whether an old lease
expired or was lost on restart. A confirmed SESSION_EXPIRED/SESSION_CONFLICT
error keeps that code, scope, outcome and requestId and provides explicit recovery
advice. Unknown fields never imply readiness.

Provider diagnostics are allowlisted schemas. Raw error messages, token values,
config contents and caller commands are not copied. `issue` preserves safe
code/scope/outcome/requestId; top-level requestId/mcpRequestId correlate the MCP
invocation. Registration/context execution errors also include stage/nextActions,
while retaining the original safe error envelope. Timeout/partial diagnosis never
schedules recovery, claims, binds, renews a lease, or changes work/presence.

Before MCP is connected: run CLI init → auth login → team select → agent create/use
with an explicitly chosen project/tool; run start, then configure the Host using
only the absolute DEVLINK_BROKER_FILE path and restart the Host. Inspect owned
Agents/pending state before an explicit recovery; never automatically repeat
creation or token issuance after an unknown result.

Exported `snapshot.schema.json` describes Rust data and `connection.schema.json`
adds the proven MCP process state. Readiness formulas also run in Zod at runtime.

A parsed descriptor alone does not establish valid credentials: failed or timed-out
broker confirmation leaves `credentials=unknown`. Missing files, malformed/private
snapshots and unobservable I/O are distinct. Advice to repair a cached descriptor
also requires reopening/restarting MCP; running processes never silently reload
rotated credentials. A settings deadline/cancellation does not turn an unknown read
into an invalid file. Every diagnostic/context call, including local file failure,
has its own requestId rather than a shared settings placeholder.

Local profile/config read failure uses BROKER_CONTEXT_UNAVAILABLE (domain scope,
unknown outcome, inspect_profile_then_rebind) rather than blaming the central
network. A settings/credential observation timeout offers retry_diagnosis without
running it. Missing credentials offer prepare_cli_profile; unknown is kept distinct
from invalid. Definite cached descriptor repair additionally requires a Host restart.

## -141 verification (2026-11-02, Asia/Seoul)

MCP full regression: 137 passed. Rust broker library: 69 passed (serial).
The actual SDK/native Rust/isolated Spring+PostgreSQL run verified diagnostic
credentials/identity/central state, unbound/bound transition and task roundtrip;
final local-error/advice deltas were then covered by the complete unit regressions.
Claude Code CLI actual `claude-opus-5-5`, effort `high`, reviewed three rounds.
Correlation, unknown credential state, cancellation, local-vs-central failure and
advice findings were fixed. Final focused review reported no actionable defects.
Remaining optional handler/issue-shape coverage suggestions are not claimed as
executed real Host evidence; real Codex/Claude acceptance remains -144.

## No-argument registration and recovery (S15P31D108-142)

After explicit CLI login/team/project/Agent preparation, use
get_connection_status({}) → get_my_context({}) → register_agent({}).
Name, toolType and role are independently optional; supplied values must exactly
match central Agent metadata. Blank/null/unknown fields are rejected, and mismatch
never edits or creates an Agent. Existing complete-input examples remain compatible.

All automatic MCP claims use recover from the first attempt, including legacy Agent
selectors, so a new registration cannot steal an active owner. Queued/concurrent
registration reuses the existing confirmed ID. Broker/MCP restart retains the
existing recovery policy: the first valid recovery owns the slot. Recovery does
not replay Agent confirmation or any business request. Parallel business bootstrap
cannot join an implicit fresh claim. Only Rust's explicit fresh API can intentionally
replace an owner; MCP no longer does so automatically.

Rust verifies the startup profile/project/origin binding both before confirmation
and after the authenticated result, before metadata matching. Changed context is
rejected; unobservable files return BROKER_CONTEXT_UNAVAILABLE. Pending/known IDs
and tokens remain in existing private CLI state; no creation or issuance API is
added. Use existing explicit CLI inspection/recovery for pending or token failures.
Only an explicit register_agent binds; the new read tools do not bind implicitly.

For inspect_connection_owner: check that the other process is yours and whether it
is still alive. After a crash without DELETE, wait for the old 90-second broker-clock
lease to expire, then restart/reopen this MCP Host. SESSION_CONFLICT remains terminal
inside the losing process; it does not silently retry or steal an active owner.
A live owner's heartbeat keeps its lease, so waiting is not a way to force it away.
Rust also compares the confirmed Agent/user/team against the startup profile,
after checking the live session and before returning registration data.

## -142 verification (2026-11-02, Asia/Seoul)

MCP full regression: 143 passed. Rust library: 70 passed; actual HTTP provider:
22 passed, 2 ignored. Actual SDK/native Rust/isolated Spring+PostgreSQL verified
empty-argument repeated/concurrent registration, legacy inputs, metadata mismatch,
unchanged stored token and existing task roundtrip. The final automatic-claim
policy and startup-profile comparison were additionally verified by full unit suites.
Claude Code CLI actual claude-opus-5-5/high reviewed three rounds. Core and Host
fixture policy findings were fixed; final review had no blocking findings. Its
remaining stale wording and recovery-wire predicate suggestions were also applied.
Actual Host acceptance of the new workflow remains -144, distinct from fixture
coverage or historical first-MVP Host results.

## Host configuration provider (S15P31D108-143)

The Rust library exposes McpConfig.plan/apply/inspect for project Codex and Claude
configuration. CLI -111 calls that provider; it owns the full setup orchestration.
See [the public provider contract](../../../contracts/setup/v1/mcp-config-provider.md)
for exact versions, Node launch argv, path/permission checks, digest conflicts,
private backups, conditional restoration and the Rust demonstration harness.
The Host receives only DEVLINK_BROKER_FILE; never copy any credential values.
After config inspection, reopen the trusted Host and explicitly call the two read
tools and register_agent({}). File applied, Host connected and task roundtrip are
separate checks. Other MCP items/settings remain present; JSON large numbers
retain their original values, and only the safe DevLink diff is exposed.
