import { randomUUID } from 'node:crypto';
import { Agent, request as requestHttp } from 'node:http';
import { BrokerFault, type FaultCode } from './contracts.js';
import type { BrokerSettings } from './settings.js';
import { safeIdentifier } from './faults.js';
import { brokerEnvelopeSchema, type BrokerResponse, type RemoteError } from './envelopes.js';
import { DEFAULT_REQUEST_TIMEOUT_MS, validateTimeout } from './call-budget.js';

const sessionCodes = new Set<FaultCode>([
  'SESSION_REPLACED', 'SESSION_CONFLICT', 'SESSION_EXPIRED', 'AGENT_UNREGISTERED',
]);
const domainCodes = new Set<FaultCode>([
  'BROKER_CONTEXT_UNAVAILABLE',
  'CENTRAL_UNAUTHENTICATED', 'BROKER_FORBIDDEN', 'VALIDATION_FAILED', 'RECEIVER_NOT_FOUND', 'RECEIVER_OFFLINE',
  'TASK_NOT_FOUND', 'TASK_STATE_CONFLICT', 'TASK_VERSION_CONFLICT', 'TASK_SAVE_FAILED',
  'TASK_CONVERSATION_UNAVAILABLE', 'INVALID_QUESTION_REFERENCE', 'USER_LOGIN_REQUIRED', 'LOCAL_ACCOUNT_BUSY',
  'RESOURCE_NOT_FOUND', 'REQUEST_KEY_CONFLICT', 'BROKER_STORAGE_FAILED', 'LOCAL_UPDATES_UNAVAILABLE',
]);
const centralTransportCodes = new Set<FaultCode>(['CENTRAL_UNAVAILABLE', 'CENTRAL_TIMEOUT', 'CENTRAL_PROTOCOL_ERROR', 'CENTRAL_RESULT_UNKNOWN']);

function remoteFault(status: number, remote: RemoteError | undefined, requestId: string, requestKey: string | undefined, control: boolean): BrokerFault {
  let code: FaultCode = status === 403 ? 'BROKER_FORBIDDEN' : 'BROKER_REJECTED';
  let known = status === 403;
  const classifiedSession = remote && (!remote.scope || remote.scope === 'session') && sessionCodes.has(remote.code as FaultCode);
  if (control && status === 403 && !classifiedSession) { code = 'LOCAL_AUTH_FORBIDDEN'; known = true; }
  else if (remote?.scope === 'local_auth') { code = status === 401 ? 'BROKER_UNAUTHORIZED' : 'LOCAL_AUTH_FORBIDDEN'; known = true; }
  else if (remote) {
    const candidate = remote.code === 'FORBIDDEN' ? 'BROKER_FORBIDDEN' : remote.code as FaultCode;
    if ((!remote.scope || remote.scope === 'session') && sessionCodes.has(candidate)) { code = candidate; known = true; }
    else if ((!remote.scope || remote.scope === 'domain') && domainCodes.has(candidate)) { code = candidate; known = true; }
    else if (remote.scope === 'transport' && centralTransportCodes.has(candidate)) { code = candidate; known = true; }
  }
  if (!known && (status >= 500 || status < 400)) code = 'BROKER_SERVER_ERROR';
  const base = new BrokerFault(code, requestId);
  const uncertain = base.outcome === 'unknown' || remote?.outcome === 'unknown'
    || (status >= 500 && !(known && remote?.scope === 'domain' && remote.outcome === 'rejected'))
    || (!known && status < 400);
  // Recipient rejection is explicitly before storage: never imply a new saved task.
  const taskId = ['RECEIVER_NOT_FOUND', 'RECEIVER_OFFLINE'].includes(code) ? undefined : remote?.taskId;
  return new BrokerFault(code, requestId, taskId, {
    ...(requestKey ? { requestKey } : {}), outcome: uncertain ? 'unknown' : 'rejected',
    ...(remote?.scope ? { scope: remote.scope } : {}),
  });
}

export class BrokerHttp {
  private readonly pending = new Set<AbortController>();
  private closed = false;
  constructor(private readonly settings: BrokerSettings, private readonly timeoutMs = DEFAULT_REQUEST_TIMEOUT_MS) { validateTimeout(timeoutMs); }
  get pendingCount(): number { return this.pending.size; }

  async request(path: string, payload: Record<string, unknown>, options: Parameters<BrokerHttp['exchange']>[2] = {}): Promise<Record<string, unknown>> {
    return (await this.exchange(path, payload, options)).data;
  }

  async exchange(
    path: string,
    payload: Record<string, unknown>,
    options: { method?: 'POST' | 'DELETE' | 'GET'; signal?: AbortSignal; timeoutMs?: number; duringClose?: boolean } = {},
  ): Promise<BrokerResponse> {
    const requestId = typeof payload.requestId === 'string' ? payload.requestId : randomUUID();
    const control = /^\/v1\/(?:sessions(?:\/|$)|context$|connection$)/.test(path);
    if (this.closed && !options.duringClose) throw new BrokerFault('CLIENT_CLOSED', requestId);
    if (!path.startsWith('/v1/') || path.startsWith('//') || path.includes('..')) throw new BrokerFault('BROKER_PROTOCOL_ERROR', requestId);
    const controller = new AbortController();
    const cancel = () => controller.abort();
    if (options.signal?.aborted) controller.abort();
    options.signal?.addEventListener('abort', cancel, { once: true });
    let timedOut = false;
    const timer = setTimeout(() => { timedOut = true; controller.abort(); }, validateTimeout(options.timeoutMs ?? this.timeoutMs));
    this.pending.add(controller);
    // Credentials must never pass through an ambient HTTP(S)_PROXY or a global fetch dispatcher.
    const agent = new Agent({ keepAlive: false, proxyEnv: {} });
    let responseStatus: number | undefined;
    try {
      const body = options.method === 'GET' ? '' : JSON.stringify({ ...payload, requestId });
      if (Buffer.byteLength(body) > 131_072) throw new BrokerFault('BROKER_PROTOCOL_ERROR', requestId);
      const response = await new Promise<{ status: number; body: Buffer }>((resolve, reject) => {
        const request = requestHttp(`${this.settings.brokerUrl}${path}`, {
          agent, method: options.method ?? 'POST', signal: controller.signal,
          headers: {
            authorization: `Bearer ${this.settings.localToken}`, 'content-type': 'application/json',
            // Node does not automatically frame a DELETE body like POST. Rust
            // needs the JSON instanceId/requestId for authenticated release.
            'content-length': Buffer.byteLength(body),
            ...(options.method === 'GET' ? { 'x-request-id': requestId, 'x-instance-id': String(payload.instanceId ?? '') } : {}),
          },
        }, (incoming) => {
          const status = incoming.statusCode ?? 0;
          responseStatus = status;
          incoming.on('error', reject);
          if (status >= 300 && status < 400) {
            reject(new BrokerFault('BROKER_PROTOCOL_ERROR', requestId));
            incoming.destroy();
            return;
          }
          const chunks: Buffer[] = [];
          let length = 0;
          incoming.on('data', (chunk: Buffer) => {
            length += chunk.length;
            if (length > 262_144) {
              reject(new BrokerFault('BROKER_PROTOCOL_ERROR', requestId));
              incoming.destroy();
            } else chunks.push(chunk);
          });
          incoming.on('end', () => resolve({ status, body: Buffer.concat(chunks) }));
        });
        request.on('error', reject);
        request.end(body);
      });
      const parsed = brokerEnvelopeSchema.safeParse(JSON.parse(response.body.toString('utf8')));
      if (!parsed.success || parsed.data.requestId !== requestId) throw new BrokerFault('BROKER_PROTOCOL_ERROR', requestId);
      const envelope = parsed.data;
      const remote = 'error' in envelope ? envelope.error : undefined;
      const data = 'data' in envelope ? envelope.data : undefined;
      const keys = [envelope.requestKey, remote?.requestKey].filter((key): key is string => key !== undefined);
      if (new Set(keys).size > 1) throw new BrokerFault('BROKER_PROTOCOL_ERROR', requestId);
      // A stored task's creation key is payload, not correlation for later reads/updates.
      const creationKey = ['create_task', 'register_agent'].includes(String(payload.operation)) ? safeIdentifier(data?.requestKey) : undefined;
      const requestKey = keys[0] ?? creationKey;
      if (response.status === 401) {
        throw new BrokerFault('BROKER_UNAUTHORIZED', requestId, remote?.taskId, { scope: 'local_auth', ...(requestKey ? { requestKey } : {}) });
      }
      if (response.status < 200 || response.status >= 300 || remote) {
        const fault = remoteFault(response.status, remote, requestId, requestKey, control);
        if (data) throw new BrokerFault(fault.code, requestId, safeIdentifier(data.taskId), {
          ...(requestKey ? { requestKey } : {}), outcome: 'unknown',
        });
        throw fault;
      }
      if (!data) throw new BrokerFault('BROKER_PROTOCOL_ERROR', requestId);
      return { data, ...(requestKey ? { requestKey } : {}) };
    } catch (error) {
      // HTTP 401 is a credential denial even if its body is malformed or interrupted.
      if (responseStatus === 401 && !(error instanceof BrokerFault && error.code === 'BROKER_UNAUTHORIZED')) throw new BrokerFault('BROKER_UNAUTHORIZED', requestId);
      if (responseStatus === 403 && control && !(error instanceof BrokerFault && (error.code === 'LOCAL_AUTH_FORBIDDEN' || sessionCodes.has(error.code)))) throw new BrokerFault('LOCAL_AUTH_FORBIDDEN', requestId);
      if (error instanceof BrokerFault) throw error;
      if (timedOut) throw new BrokerFault('BROKER_TIMEOUT', requestId);
      if (controller.signal.aborted) throw new BrokerFault(this.closed ? 'CLIENT_CLOSED' : 'REQUEST_ABORTED', requestId);
      if (responseStatus === 403) throw new BrokerFault(control ? 'LOCAL_AUTH_FORBIDDEN' : 'BROKER_FORBIDDEN', requestId);
      if (error instanceof SyntaxError) throw new BrokerFault('BROKER_PROTOCOL_ERROR', requestId);
      if (error && typeof error === 'object' && 'code' in error && ['ECONNRESET', 'ERR_STREAM_PREMATURE_CLOSE', 'EPIPE'].includes(String(error.code))) {
        throw new BrokerFault('BROKER_CONNECTION_CLOSED', requestId);
      }
      throw new BrokerFault('BROKER_UNAVAILABLE', requestId);
    } finally {
      clearTimeout(timer);
      options.signal?.removeEventListener('abort', cancel);
      this.pending.delete(controller);
      agent.destroy();
    }
  }

  close(): void {
    this.closed = true;
    for (const controller of this.pending) controller.abort();
  }
}
