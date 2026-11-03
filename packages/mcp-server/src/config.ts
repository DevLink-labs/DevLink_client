import { parseArgs } from 'node:util';
import { z } from 'zod';
import { MAX_TIMEOUT_MS } from './broker/call-budget.js';

const timeout = z.number().int().min(1).max(MAX_TIMEOUT_MS).optional();

const configSchema = z.object({
  transport: z.literal('stdio'),
  logLevel: z.enum(['debug', 'info', 'error']).default('info'),
  brokerSettingsPath: z.string().optional(),
  requestTimeoutMs: timeout, callTimeoutMs: timeout,
});

export type RuntimeConfig = Readonly<z.infer<typeof configSchema>>;

export class ConfigError extends Error {
  readonly code = 'CONFIG_INVALID';

  constructor(readonly fields: readonly string[]) {
    super('Invalid MCP runtime configuration');
    this.name = 'ConfigError';
  }
}

/** Only read runtime settings; never copy credentials or arbitrary environment values. */
export function loadConfig(
  args: string[] = process.argv.slice(2),
  env: NodeJS.ProcessEnv = process.env,
): RuntimeConfig {
  let values;
  try {
    ({ values } = parseArgs({
      args,
      options: {
        transport: { type: 'string' },
        'log-level': { type: 'string' },
        'request-timeout-ms': { type: 'string' },
        'call-timeout-ms': { type: 'string' },
      },
      strict: true,
      allowPositionals: false,
    }));
  } catch {
    // parseArgs errors can echo input values, so expose only a safe field label.
    throw new ConfigError(['arguments']);
  }

  const requestTimeout = values['request-timeout-ms'] ?? env.DEVLINK_MCP_REQUEST_TIMEOUT_MS;
  const callTimeout = values['call-timeout-ms'] ?? env.DEVLINK_MCP_CALL_TIMEOUT_MS;
  const milliseconds = (value: string) => /^[1-9][0-9]*$/.test(value) ? Number(value) : NaN;
  const parsed = configSchema.safeParse({
    transport: values.transport ?? env.DEVLINK_MCP_TRANSPORT,
    logLevel: values['log-level'] ?? env.DEVLINK_MCP_LOG_LEVEL,
    ...(env.DEVLINK_BROKER_FILE !== undefined ? { brokerSettingsPath: env.DEVLINK_BROKER_FILE } : {}),
    ...(requestTimeout !== undefined ? { requestTimeoutMs: milliseconds(requestTimeout) } : {}),
    ...(callTimeout !== undefined ? { callTimeoutMs: milliseconds(callTimeout) } : {}),
  });
  if (!parsed.success) {
    throw new ConfigError([...new Set(parsed.error.issues.map((issue) => String(issue.path[0])))]);
  }
  return Object.freeze(parsed.data);
}
