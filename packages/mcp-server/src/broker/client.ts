import { randomUUID } from 'node:crypto';
import { setMaxListeners } from 'node:events';
import { BrokerFault, operationRoutes, terminalSessionFaults,
  type BrokerOperation, type BrokerResult, type FaultCode } from './contracts.js';
import { identifierSchema, immutableSession, sessionSchema, type BrokerSession } from './schemas.js';
import { BrokerHttp } from './http.js';
import { readBrokerSettings, validateSettings, SettingsFault, type BrokerSettings } from './settings.js';
import type { Logger } from '../logger.js';
import { safeIdentifier, type Correlation } from './faults.js';
import { CallBudget, DEFAULT_CALL_TIMEOUT_MS, DEFAULT_REQUEST_TIMEOUT_MS, validateTimeout } from './call-budget.js';
import { contextSchema, connectionSchema } from './onboarding.js';

export interface BrokerClientOptions {
  settingsPath?: string;
  settings?: BrokerSettings;
  requestTimeoutMs?: number;
  callTimeoutMs?: number;
  shutdownTimeoutMs?: number;
  heartbeatMs?: number;
  retryBaseMs?: number;
  retryLimit?: number;
  logger?: Logger;
}
type CallOptions = { signal?: AbortSignal; mcpRequestId?: string | number };
const actorFields = ['userId', 'teamId', 'agentId', 'sessionId', 'principal', 'context'];
function correlatedFault(error: unknown, requestId: string): BrokerFault {
  return error instanceof BrokerFault ? new BrokerFault(error.code, requestId, error.taskId, {
    outcome: error.outcome, ...(error.scope ? { scope: error.scope } : {}), ...(error.requestKey ? { requestKey: error.requestKey } : {}),
  }) : new BrokerFault('BROKER_PROTOCOL_ERROR', requestId);
}

/** One client per MCP process; only authenticated broker responses may change identity. */
export class BrokerClient {
  private readonly instanceId = randomUUID();
  private transportPromise: Promise<BrokerHttp> | undefined;
  private http: BrokerHttp | undefined;
  private session: BrokerSession | undefined;
  private identity: Readonly<{ userId: string; teamId: string }> | undefined;
  private agentId: string | undefined;
  private pendingAgentId: string | undefined;
  private toolType: string | undefined;
  private connecting: Promise<BrokerSession> | undefined;
  private registrationTail: Promise<void> | undefined;
  private epoch = 0;
  private terminal: FaultCode | undefined;
  private terminalDetails: Pick<BrokerFault, 'scope' | 'outcome'> | undefined;
  private timer: ReturnType<typeof setTimeout> | undefined;
  private retryCount = 0;
  private closed = false;
  private closePromise: Promise<void> | undefined;
  private readonly lifetime = new AbortController();
  private observedContext: string | undefined;
  private contextConstraint: Readonly<{ userId: string; teamId: string; agentId: string }> | undefined;
  private contextBlocked = false;
  /** False after a broker rejected the claim's processId field (older broker, #108). */
  private sendProcessId = true;

  constructor(private readonly options: BrokerClientOptions = {}) {
    validateTimeout(options.requestTimeoutMs ?? DEFAULT_REQUEST_TIMEOUT_MS);
    validateTimeout(options.callTimeoutMs ?? DEFAULT_CALL_TIMEOUT_MS);
    validateTimeout(options.shutdownTimeoutMs ?? 1000);
    setMaxListeners(0, this.lifetime.signal);
  }

  get pendingCount(): number { return this.http?.pendingCount ?? 0; }
  get state(): string { return this.closed ? 'closed' : this.terminal ?? (this.session?.principal.agentId ? 'bound' : this.session ? 'unbound' : 'inactive'); }

  async getConnectionStatus(options: CallOptions = {}): Promise<BrokerResult> {
    const requestId = randomUUID();
    const budget = new CallBudget(requestId, this.options.callTimeoutMs ?? DEFAULT_CALL_TIMEOUT_MS, options.signal, this.lifetime.signal);
    let credentialsChecked = false;
    const result = (data: Record<string, unknown>) => ({ requestId,
      ...(options.mcpRequestId !== undefined ? { mcpRequestId: options.mcpRequestId } : {}),
      data: this.withClientIssue(data, requestId) });
    try {
      budget.check();
      const http = await budget.wait(this.transport()); credentialsChecked = true;
      const raw = await http.request('/v1/connection', { requestId, instanceId: this.instanceId }, { method: 'GET', signal: budget.signal });
      budget.check();
      // This field belongs to this MCP process, never to a provider response.
      if (Object.hasOwn(raw, 'clientIssue')) throw new BrokerFault('BROKER_PROTOCOL_ERROR', requestId);
      const parsed = connectionSchema.safeParse({ ...raw, mcp: 'running' });
      if (!parsed.success) throw new BrokerFault('BROKER_PROTOCOL_ERROR', requestId);
      return result(parsed.data);
    } catch (error) {
      const mapped = budget.map(error);
      const fault = correlatedFault(mapped, requestId);
      if (['CLIENT_CLOSED', 'REQUEST_ABORTED'].includes(fault.code)) throw fault;
      let credentials: 'missing' | 'invalid' | 'unknown' = mapped instanceof SettingsFault ? mapped.credentialState
        : !credentialsChecked && this.options.settings && fault.code === 'CREDENTIAL_FILE_INVALID' ? 'invalid' : 'unknown';
      const denied = ['BROKER_UNAUTHORIZED', 'LOCAL_AUTH_FORBIDDEN'].includes(fault.code);
      if (denied) credentials = 'invalid';
      const scope = fault.scope ?? (!credentialsChecked ? 'credentials' : denied ? 'local_auth'
        : fault.category === 'transport' ? 'transport' : fault.category === 'protocol' ? 'protocol' : 'domain');
      const issue = { code: fault.code, scope, outcome: fault.outcome, requestId: fault.requestId };
      const sessionConflict = ['SESSION_CONFLICT', 'SESSION_REPLACED'].includes(fault.code);
      const sessionExpired = fault.code === 'SESSION_EXPIRED';
      const data = connectionSchema.parse({ mcp: 'running', observedAt: new Date().toISOString(),
        broker: scope === 'protocol' ? 'unhealthy' : 'unknown', central: 'unknown', identity: 'unknown', credentials,
        mcpConfig: 'unknown', hostSession: sessionConflict ? 'conflict' : sessionExpired ? 'unbound' : 'not_checked', setupReady: false, collaborationReady: false, issue,
        nextActions: [!credentialsChecked ? credentials === 'missing' ? 'prepare_cli_profile' : credentials === 'invalid' ? 'repair_private_file' : fault.code === 'BROKER_TIMEOUT' ? 'retry_diagnosis' : 'open_or_restart_host'
          : denied ? 'repair_private_file' : sessionConflict ? 'inspect_connection_owner' : sessionExpired ? 'register_agent'
            : fault.code === 'CENTRAL_UNAUTHENTICATED' ? 'stop_then_reauthenticate_agent'
              : fault.code === 'BROKER_CONTEXT_UNAVAILABLE' ? 'inspect_profile_then_rebind'
              : fault.code.startsWith('CENTRAL_') ? 'check_network_then_retry'
                : scope === 'protocol' ? 'check_provider_contract' : 'inspect_local_broker',
          ...((!credentialsChecked && credentials !== 'unknown') || denied ? ['open_or_restart_host'] : [])] });
      // Never observe/retry/claim here, including when a previous business session is terminal.
      return result(data);
    } finally { budget.dispose(); }
  }

  private withClientIssue(data: Record<string, unknown>, requestId: string): Record<string, unknown> {
    // Read after the HTTP await: a concurrent context/registration call may have blocked us.
    const code = this.contextBlocked ? 'BROKER_CONTEXT_INVALID' : this.terminal;
    if (!code) return data;
    const fault = new BrokerFault(code, requestId);
    const details = this.contextBlocked ? undefined : this.terminalDetails;
    const localAuth = ['BROKER_UNAUTHORIZED', 'LOCAL_AUTH_FORBIDDEN'].includes(code);
    const conflict = ['SESSION_CONFLICT', 'SESSION_REPLACED'].includes(code);
    const scope = details?.scope ?? (localAuth ? 'local_auth' : conflict ? 'session'
      : code === 'CREDENTIAL_FILE_INVALID' ? 'credentials' : 'domain');
    const action = conflict ? 'inspect_connection_owner' : localAuth ? 'repair_private_file'
      : code === 'CREDENTIAL_FILE_INVALID' ? data.credentials === 'invalid' ? 'repair_private_file'
        : data.credentials === 'missing' ? 'prepare_cli_profile' : undefined
        : 'inspect_profile_then_rebind';
    const nextActions = [...new Set([...(action ? [action] : []),
      ...(data.nextActions as string[]).filter(value => !['register_agent', 'open_or_restart_host'].includes(value)),
      'open_or_restart_host'])];
    // Preserve live provider stages/issue. Environment setup can be ready while this
    // process is blocked; collaboration must remain false until a new Host is opened.
    return connectionSchema.parse({ ...data, collaborationReady: false, nextActions,
      clientIssue: { code, scope, outcome: details?.outcome ?? fault.outcome, requestId } });
  }

  async getMyContext(options: CallOptions = {}): Promise<BrokerResult> {
    const requestId = randomUUID();
    const budget = new CallBudget(requestId, this.options.callTimeoutMs ?? DEFAULT_CALL_TIMEOUT_MS, options.signal, this.lifetime.signal);
    try {
      budget.check();
      if (this.terminal) throw new BrokerFault(this.terminal, requestId);
      const http = await budget.wait(this.transport());
      const raw = await http.request('/v1/context', { requestId, instanceId: this.instanceId }, { method: 'GET', signal: budget.signal });
      budget.check();
      const parsed = contextSchema.safeParse(raw);
      if (!parsed.success) throw new BrokerFault('BROKER_CONTEXT_INVALID', requestId);
      const data = parsed.data;
      if (data.principal) {
        const p = data.principal;
        const context = JSON.stringify([p.userId, p.teamId, p.agentId, p.toolType, data.project]);
        if ((this.observedContext && context !== this.observedContext)
          || (this.identity && (p.userId !== this.identity.userId || p.teamId !== this.identity.teamId))
          || (this.agentId && p.agentId !== this.agentId)) {
          this.contextBlocked = true;
          throw new BrokerFault('BROKER_CONTEXT_INVALID', requestId);
        }
        this.observedContext ??= context;
        this.contextConstraint ??= Object.freeze({ userId: p.userId, teamId: p.teamId, agentId: p.agentId });
      }
      return { requestId, ...(options.mcpRequestId !== undefined ? { mcpRequestId: options.mcpRequestId } : {}), data };
    } catch (error) {
      const mapped = budget.map(error);
      // Diagnostic failures never schedule a heartbeat or recover/claim a session.
      throw correlatedFault(mapped, requestId);
    } finally { budget.dispose(); }
  }

  private async transport(): Promise<BrokerHttp> {
    this.transportPromise ??= (async () => {
      let settings: BrokerSettings;
      try {
        settings = this.options.settings ? validateSettings(this.options.settings)
          : await readBrokerSettings(this.options.settingsPath ?? '');
      } catch (error) { throw error instanceof BrokerFault ? error : new SettingsFault('unknown'); }
      if (this.closed) throw new BrokerFault('CLIENT_CLOSED', 'settings');
      this.agentId ??= settings.agentId;
      return this.http = new BrokerHttp(settings, this.options.requestTimeoutMs);
    })();
    return this.transportPromise;
  }

  private schedule(delay = this.options.heartbeatMs ?? 30_000): void {
    clearTimeout(this.timer);
    if (this.closed || this.terminal) return;
    this.timer = setTimeout(() => { void this.heartbeat().catch(() => {}); }, delay);
    this.timer.unref();
  }

  private discard(snapshot?: BrokerSession): void {
    if (snapshot && this.session !== snapshot) return;
    this.session = undefined;
    ++this.epoch;
    clearTimeout(this.timer);
  }

  private observe(error: unknown, snapshot?: BrokerSession): void {
    if (!(error instanceof BrokerFault) || this.closed) return;
    // Old requests may report errors, but cannot revoke a newer binding or lease.
    if (snapshot && this.session !== snapshot) return;
    if (terminalSessionFaults.has(error.code)) {
      this.terminal = error.code;
      this.terminalDetails = { scope: error.scope, outcome: error.outcome };
      this.discard();
    } else if (['BROKER_UNAVAILABLE', 'BROKER_CONNECTION_CLOSED', 'BROKER_TIMEOUT', 'SESSION_EXPIRED'].includes(error.code)) {
      this.discard();
      this.retry();
    }
  }

  private retry(): void {
    if (this.retryCount < (this.options.retryLimit ?? 5)) {
      this.schedule(Math.min((this.options.retryBaseMs ?? 1000) * 2 ** this.retryCount++, 30_000));
    }
  }

  private checkedSession(value: unknown, requestId: string, refresh = false): BrokerSession {
    const result = sessionSchema.safeParse(value);
    if (!result.success) throw new BrokerFault('BROKER_CONTEXT_INVALID', requestId);
    const { principal } = result.data;
    if ((this.identity && (principal.userId !== this.identity.userId || principal.teamId !== this.identity.teamId))
      || (this.contextConstraint && (principal.userId !== this.contextConstraint.userId || principal.teamId !== this.contextConstraint.teamId
        || (principal.agentId !== undefined && principal.agentId !== this.contextConstraint.agentId)))
      || (this.agentId && principal.agentId !== this.agentId)
      || (this.pendingAgentId && principal.agentId && principal.agentId !== this.pendingAgentId)
      || (this.toolType && (!principal.toolType || (!refresh && principal.toolType !== this.toolType)))) {
      throw new BrokerFault('BROKER_CONTEXT_INVALID', requestId);
    }
    return immutableSession(result.data);
  }

  private publish(session: BrokerSession): void {
    this.identity ??= Object.freeze({ userId: session.principal.userId, teamId: session.principal.teamId });
    this.agentId = session.principal.agentId ?? this.agentId;
    if (session.principal.agentId) this.pendingAgentId = undefined;
    this.toolType = session.principal.toolType ?? this.toolType;
    this.session = session;
    ++this.epoch;
    this.schedule();
  }

  private assertCurrent(snapshot: BrokerSession, requestId: string): void {
    if (this.closed) throw new BrokerFault('CLIENT_CLOSED', requestId);
    if (this.terminal) throw new BrokerFault(this.terminal, requestId);
    if (this.session !== snapshot) throw new BrokerFault('SESSION_CONTEXT_CHANGED', requestId);
  }

  private ensureSession(): Promise<BrokerSession> {
    if (this.closed) return Promise.reject(new BrokerFault('CLIENT_CLOSED', 'session'));
    if (this.terminal) return Promise.reject(new BrokerFault(this.terminal, 'session'));
    if (this.session) return Promise.resolve(this.session);
    this.connecting ??= (async () => {
      const epoch = this.epoch;
      const http = await this.transport();
      const requestId = randomUUID();
      const mode = 'recover';
      // #108: with this process ID the broker ends the session as soon as this process exits,
      // even when a Host kills it without closing stdin. A broker older than that field rejects
      // the request (VALIDATION_FAILED, nothing created); claim once more without it.
      const claim = (withProcessId: boolean) => http.request('/v1/sessions', {
        requestId, instanceId: this.instanceId, mode,
        ...((this.agentId ?? this.pendingAgentId) ? { agentId: this.agentId ?? this.pendingAgentId } : {}),
        ...(withProcessId ? { processId: process.pid } : {}),
      });
      let data: unknown;
      try {
        data = await claim(this.sendProcessId);
      } catch (error) {
        if (!this.sendProcessId || !(error instanceof BrokerFault) || error.code !== 'VALIDATION_FAILED') throw error;
        data = await claim(false);
        // Stop sending the field only once a claim without it was accepted (an older broker).
        // If the retry fails too, keep sending it. A transient 408/413/415, which also reads as
        // VALIDATION_FAILED, followed by an accepted retry would still turn it off; on a local
        // loopback with a small body that is not expected.
        this.sendProcessId = false;
      }
      const session = this.checkedSession(data, requestId);
      if (this.closed) throw new BrokerFault('CLIENT_CLOSED', requestId);
      if (this.terminal) throw new BrokerFault(this.terminal, requestId);
      if (epoch !== this.epoch) throw new BrokerFault('SESSION_CONTEXT_CHANGED', requestId);
      this.publish(session);
      this.options.logger?.debug('broker.session_ready', { requestId });
      return session;
    })().catch((error: unknown) => {
      // A shared connection failure consumes one retry, regardless of waiter count.
      this.observe(error);
      throw error;
    }).finally(() => { this.connecting = undefined; });
    return this.connecting;
  }

  async heartbeat(): Promise<void> {
    let snapshot: BrokerSession | undefined;
    try {
      snapshot = await this.ensureSession();
      const http = await this.transport();
      await http.request(`/v1/sessions/${encodeURIComponent(snapshot.sessionId)}/heartbeat`, {
        requestId: randomUUID(), instanceId: this.instanceId,
      });
      if (this.session === snapshot) {
        this.retryCount = 0;
        this.schedule();
      }
    } catch (error) { if (snapshot) this.observe(error, snapshot); throw error; }
  }

  private enqueueRegistration(run: () => Promise<BrokerResult>): Promise<BrokerResult> {
    const result = (this.registrationTail ?? Promise.resolve()).then(run);
    const settled = result.then(() => {}, () => {});
    this.registrationTail = settled;
    void settled.then(() => { if (this.registrationTail === settled) this.registrationTail = undefined; });
    return result;
  }

  private async waitForRegistration(budget: CallBudget): Promise<void> {
    while (this.registrationTail) {
      await budget.wait(this.registrationTail);
    }
  }

  async execute(operation: BrokerOperation, args: Record<string, unknown>, options: CallOptions = {}): Promise<BrokerResult> {
    const requestId = randomUUID();
    const callOptions = { ...options };
    const budget = new CallBudget(requestId, this.options.callTimeoutMs ?? DEFAULT_CALL_TIMEOUT_MS, callOptions.signal, this.lifetime.signal);
    const trace: Correlation = {};
    this.options.logger?.debug('broker.call_started', { requestId, operation });
    try {
      budget.check();
      if (!Object.hasOwn(operationRoutes, operation)) throw new BrokerFault('BROKER_PROTOCOL_ERROR', requestId);
      if (this.contextBlocked) throw new BrokerFault('BROKER_CONTEXT_INVALID', requestId);
      const input = structuredClone(args);
      const taskId = safeIdentifier(input.taskId);
      if (taskId && !['create_task', 'register_agent'].includes(operation)) trace.taskId = taskId;
      const callerKey = safeIdentifier(input.requestKey);
      if (callerKey && callerKey.length <= 64 && ['create_task', 'update_task_status', 'complete_task', 'get_task_result', 'ask_question', 'answer_question'].includes(operation)) trace.requestKey = callerKey;
      if (actorFields.some((field) => Object.hasOwn(input, field))) throw new BrokerFault('CALLER_CONTEXT_OVERRIDE', requestId);
      const run = () => this.perform(operation, input, requestId, callOptions, budget, trace);
      let result: BrokerResult;
      if (operation === 'register_agent') result = await budget.wait(this.enqueueRegistration(run));
      else {
        await this.waitForRegistration(budget);
        result = await run();
      }
      this.options.logger?.debug('broker.call_succeeded', { requestId, operation });
      return result;
    } catch (error) {
      const mapped = budget.map(error);
      const fault = new BrokerFault(mapped instanceof BrokerFault ? mapped.code : 'BROKER_PROTOCOL_ERROR', requestId, trace.taskId, {
        ...(trace.requestKey ? { requestKey: trace.requestKey } : {}), outcome: mapped instanceof BrokerFault ? mapped.outcome : 'unknown',
        ...(mapped instanceof BrokerFault && mapped.scope ? { scope: mapped.scope } : {}),
        ...(callOptions.mcpRequestId !== undefined ? { mcpRequestId: callOptions.mcpRequestId } : {}),
      });
      this.options.logger?.error('broker.call_failed', { requestId, operation, code: fault.code });
      throw fault;
    } finally { budget.dispose(); }
  }

  private async perform(operation: BrokerOperation, args: Record<string, unknown>, requestId: string, options: CallOptions,
    budget: CallBudget, trace: Correlation): Promise<BrokerResult> {
    let snapshot: BrokerSession | undefined;
    let binding = false;
    try {
      budget.check();
      snapshot = await budget.wait(this.ensureSession());
      if (operation !== 'register_agent' && !snapshot.principal.agentId) throw new BrokerFault('AGENT_UNREGISTERED', requestId);
      this.assertCurrent(snapshot, requestId);
      budget.check();
      const http = await budget.wait(this.transport());
      budget.check();
      const signalOptions = { signal: budget.signal };
      const response = await http.exchange(operationRoutes[operation], {
        requestId, operation, arguments: args,
        context: { ...snapshot.principal, sessionId: snapshot.sessionId },
        ...(options.mcpRequestId !== undefined ? { mcpRequestId: options.mcpRequestId } : {}),
      }, signalOptions).catch((error: unknown) => {
        if (error instanceof BrokerFault) {
          if (error.requestKey) trace.requestKey = error.requestKey;
          if (error.taskId) trace.taskId = error.taskId;
        }
        throw error;
      });
      const { data } = response;
      if (response.requestKey) trace.requestKey = response.requestKey;
      const savedTaskId = safeIdentifier(data.taskId);
      if (savedTaskId) trace.taskId = savedTaskId;
      let issuedAgentId: string | undefined;
      if (operation === 'register_agent') {
        const agent = identifierSchema.safeParse(data.agentId);
        const expectedAgentId = this.agentId ?? this.pendingAgentId ?? this.contextConstraint?.agentId;
        if (!agent.success || (expectedAgentId && expectedAgentId !== agent.data)) throw new BrokerFault('BROKER_CONTEXT_INVALID', requestId);
        // Preserve an ID that actually arrived, even if the overall budget just expired.
        if (!this.agentId) this.pendingAgentId = agent.data;
        issuedAgentId = agent.data;
      }
      budget.check();
      if (this.closed) throw new BrokerFault('CLIENT_CLOSED', requestId);
      if (operation === 'register_agent') {
        this.assertCurrent(snapshot, requestId);
        binding = true;
        const bound = await http.request(`/v1/sessions/${encodeURIComponent(snapshot.sessionId)}/bind`, {
          requestId: randomUUID(), agentId: issuedAgentId, instanceId: this.instanceId,
        }, signalOptions);
        budget.check();
        this.assertCurrent(snapshot, requestId);
        const session = this.checkedSession(bound, requestId, true);
        if (session.sessionId !== snapshot.sessionId || session.principal.agentId !== issuedAgentId) throw new BrokerFault('BROKER_CONTEXT_INVALID', requestId);
        this.publish(session);
      }
      this.retryCount = 0;
      return { requestId, data,
        ...(options.mcpRequestId !== undefined ? { mcpRequestId: options.mcpRequestId } : {}),
        ...(trace.requestKey ? { requestKey: trace.requestKey } : {}),
      };
    } catch (error) {
      // A caller's budget is not evidence that the shared broker session failed.
      if (snapshot) this.observe(error, snapshot);
      if (binding && this.session === snapshot) { this.discard(snapshot); this.retry(); }
      throw budget.map(error);
    }
  }

  close(): Promise<void> {
    this.closePromise ??= (async () => {
      this.closed = true;
      this.lifetime.abort();
      const session = this.session;
      this.discard();
      this.http?.close();
      if (session && this.http) {
        await this.http.request(`/v1/sessions/${encodeURIComponent(session.sessionId)}`, {
          requestId: randomUUID(), instanceId: this.instanceId,
        }, { method: 'DELETE', timeoutMs: this.options.shutdownTimeoutMs ?? 1000, duringClose: true }).catch(() => {});
      }
    })();
    return this.closePromise;
  }
}
