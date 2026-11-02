import { z } from 'zod';
import { toolTypeSchema } from './schemas.js';
import { faultCodes } from './faults.js';
export const decimalId = z.string().regex(/^[1-9][0-9]{0,18}$/)
  .refine(value => BigInt(value) <= 9_223_372_036_854_775_807n, 'ID must fit Java Long.');
export const contextSchema = z.strictObject({
  identity: z.enum(['verified', 'unavailable']),
  principal: z.strictObject({ userId: decimalId, teamId: decimalId, agentId: decimalId, toolType: toolTypeSchema.nullable() }).nullable(),
  agent: z.strictObject({ name: z.string().nullable(), roleDescription: z.string().nullable(),
    toolType: toolTypeSchema.nullable(), projectName: z.string().nullable(), source: z.literal('central_agents_me') }).nullable(),
  project: z.strictObject({ path: z.string().nullable(), source: z.enum(['bound_profile', 'unavailable']) }),
  hostSession: z.enum(['bound', 'unbound', 'conflict', 'not_checked']),
}).superRefine((v, ctx) => {
  if ((v.identity === 'verified' ? v.principal === null || v.agent === null : v.principal !== null || v.agent !== null)
    || (v.principal && v.principal.toolType !== v.agent?.toolType)
    || (v.project.source === 'bound_profile') !== (v.project.path !== null)
    || (v.identity !== 'verified' && v.hostSession !== 'not_checked')) {
    ctx.addIssue({ code: 'custom', message: 'Inconsistent context.' });
  }
});
export const contextOutput = z.strictObject({ requestId: z.string(),
  mcpRequestId: z.union([z.string(), z.number()]).optional(), data: contextSchema });

export const nextAction = z.enum(['prepare_cli_profile', 'repair_private_file', 'inspect_local_broker',
  'check_network_then_retry', 'stop_then_reauthenticate_agent', 'inspect_profile_then_rebind',
  'inspect_connection_owner', 'check_provider_contract', 'repair_inbox_then_restart', 'register_agent',
  'review_mcp_config', 'open_or_restart_host', 'inspect_owned_agents_then_use', 'retry_diagnosis']);
export const diagnosticIssue = z.strictObject({
  code: z.enum(faultCodes),
  scope: z.enum(['credentials', 'local_auth', 'session', 'transport', 'domain', 'protocol']),
  outcome: z.enum(['unknown', 'rejected']), requestId: z.string().min(1).max(128),
});
export const snapshotSchema = z.strictObject({
  observedAt: z.iso.datetime({ offset: true }),
  broker: z.enum(['running', 'stopped', 'unhealthy', 'unknown']),
  central: z.enum(['Connecting', 'Authenticating', 'Connected', 'Reconnecting', 'AuthFailed', 'Replaced', 'ProtocolViolation', 'InboxFailed', 'Disconnected', 'unknown']),
  identity: z.enum(['verified', 'unauthenticated', 'mismatch', 'unknown']),
  credentials: z.enum(['valid', 'missing', 'invalid', 'unknown']),
  mcpConfig: z.enum(['applied', 'missing', 'conflict', 'unknown']),
  hostSession: z.enum(['bound', 'unbound', 'conflict', 'not_checked', 'unknown']),
  setupReady: z.boolean(), collaborationReady: z.boolean().nullable(),
  nextActions: z.array(nextAction), issue: diagnosticIssue.optional(),
});
export const connectionSchema = snapshotSchema.extend({ mcp: z.literal('running') }).superRefine((v, ctx) => {
  const ready = v.broker === 'running' && v.central === 'Connected' && v.identity === 'verified'
    && v.credentials === 'valid' && v.mcpConfig === 'applied';
  const collaboration = !ready ? false : v.hostSession === 'bound' ? true
    : ['unbound', 'conflict'].includes(v.hostSession) ? false : null;
  if (v.setupReady !== ready || v.collaborationReady !== collaboration
    || (ready && v.issue !== undefined)
    || (v.identity === 'verified' && (v.central !== 'Connected' || v.broker !== 'running'))
    || (v.hostSession === 'bound' && v.identity !== 'verified')) {
    ctx.addIssue({ code: 'custom', message: 'Inconsistent readiness.' });
  }
});
export const connectionOutput = z.strictObject({ requestId: z.string(),
  mcpRequestId: z.union([z.string(), z.number()]).optional(), data: connectionSchema });
