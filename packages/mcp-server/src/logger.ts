import type { Writable } from 'node:stream';
import type { RuntimeConfig } from './config.js';
import { operationRoutes, type BrokerOperation } from './broker/contracts.js';

type Level = RuntimeConfig['logLevel'];
type Event =
  | 'runtime.started'
  | 'runtime.closed'
  | 'runtime.start_failed'
  | 'runtime.transport_error'
  | 'runtime.shutdown_failed'
  | 'tool.ping'
  | 'broker.call_started' | 'broker.call_succeeded' | 'broker.call_failed' | 'broker.session_ready';

type Diagnostic = { code?: string; fields?: readonly string[]; requestId?: string; operation?: BrokerOperation };

export interface Logger {
  debug(event: Event, diagnostic?: Diagnostic): void;
  info(event: Event, diagnostic?: Diagnostic): void;
  error(event: Event, diagnostic?: Diagnostic): void;
}

/** Accept known events and field names rather than arbitrary error messages or payloads. */
export function createLogger(
  stream: Pick<Writable, 'write'> = process.stderr,
  level: Level = 'info',
): Logger {
  const priority = { debug: 0, info: 1, error: 2 };
  const write = (entryLevel: Level, event: Event, diagnostic: Diagnostic = {}) => {
    if (priority[entryLevel] < priority[level]) return;
    stream.write(`${JSON.stringify({
      time: new Date().toISOString(), level: entryLevel, event,
      ...(diagnostic.code ? { code: diagnostic.code } : {}),
      ...(diagnostic.fields ? { fields: diagnostic.fields } : {}),
      ...(diagnostic.requestId && /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/i.test(diagnostic.requestId) ? { requestId: diagnostic.requestId } : {}),
      ...(diagnostic.operation && Object.hasOwn(operationRoutes, diagnostic.operation) ? { operation: diagnostic.operation } : {}),
    })}\n`);
  };
  return {
    debug: (event, diagnostic) => write('debug', event, diagnostic),
    info: (event, diagnostic) => write('info', event, diagnostic),
    error: (event, diagnostic) => write('error', event, diagnostic),
  };
}
