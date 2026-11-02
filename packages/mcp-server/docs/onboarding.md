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
