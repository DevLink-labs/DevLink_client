import { performance } from 'node:perf_hooks';
import { BrokerFault, type FaultCode } from './faults.js';

export const DEFAULT_REQUEST_TIMEOUT_MS = 10_000;
export const DEFAULT_CALL_TIMEOUT_MS = 30_000;
export const MAX_TIMEOUT_MS = 300_000;
export function validateTimeout(value: number): number {
  if (!Number.isSafeInteger(value) || value < 1 || value > MAX_TIMEOUT_MS) throw new RangeError('Invalid broker timeout configuration');
  return value;
}

/** A caller's deadline covers queueing, bootstrap and bind, without cancelling a shared claim. */
export class CallBudget {
  readonly controller = new AbortController();
  get signal(): AbortSignal { return this.controller.signal; }
  private readonly deadline: number;
  private readonly timer: ReturnType<typeof setTimeout>;
  private readonly cleanup: Array<() => void> = [];
  private reason: FaultCode = 'REQUEST_ABORTED';
  constructor(private readonly requestId: string, timeoutMs: number, caller: AbortSignal | undefined, closed: AbortSignal) {
    this.deadline = performance.now() + validateTimeout(timeoutMs);
    const watch = (signal: AbortSignal, code: FaultCode) => {
      const listener = () => this.abort(code);
      if (signal.aborted) listener();
      else { signal.addEventListener('abort', listener, { once: true }); this.cleanup.push(() => signal.removeEventListener('abort', listener)); }
    };
    watch(closed, 'CLIENT_CLOSED');
    if (caller) watch(caller, 'REQUEST_ABORTED');
    this.timer = setTimeout(() => this.abort('BROKER_TIMEOUT'), timeoutMs);
    this.timer.unref();
  }
  private abort(code: FaultCode): void {
    if (!this.signal.aborted) { this.reason = code; this.controller.abort(); }
  }
  check(): void {
    if (performance.now() >= this.deadline) this.abort('BROKER_TIMEOUT');
    if (this.signal.aborted) throw new BrokerFault('REQUEST_ABORTED', this.requestId);
  }
  map(error: unknown): unknown {
    if (this.signal.aborted && error instanceof BrokerFault && error.code === 'REQUEST_ABORTED') {
      return new BrokerFault(this.reason, this.requestId, error.taskId, {
        ...(error.requestKey ? { requestKey: error.requestKey } : {}), outcome: 'unknown',
      });
    }
    return error;
  }
  wait<T>(result: Promise<T>): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      const cancel = () => { try { this.check(); } catch (error) { reject(error); } };
      if (this.signal.aborted) cancel();
      else this.signal.addEventListener('abort', cancel, { once: true });
      void result.then((value) => { this.signal.removeEventListener('abort', cancel); resolve(value); },
        (error: unknown) => { this.signal.removeEventListener('abort', cancel); reject(error); });
    });
  }
  dispose(): void { clearTimeout(this.timer); for (const cleanup of this.cleanup) cleanup(); }
}
