import { z } from 'zod';
import type { BrokerClient } from '../broker/client.js';
import { contextOutput, connectionOutput } from '../broker/onboarding.js';
import { defineTool } from './registry.js';
export function onboardingTools(broker: Pick<BrokerClient, 'getMyContext' | 'getConnectionStatus'>) {
  return [defineTool({ name: 'get_my_context',
    description: 'Read verified user/team/agent and project provenance without claiming, binding or renewing a session. Unavailable fields are not inferred. agent.part and agent.roles (the role tags find_agent matches) appear only when central reported them: a missing part or roles is unknown, not OTHER or no tags, while roles [] means no tags. Reconfirm the CLI profile after a context change.',
    inputSchema: z.strictObject({}), outputSchema: contextOutput,
    annotations: { readOnlyHint: true, destructiveHint: false, idempotentHint: true, openWorldHint: true },
    handler: (_args, ctx) => broker.getMyContext({ signal: ctx.signal, mcpRequestId: ctx.mcpRequestId }),
  }), defineTool({ name: 'get_connection_status',
    description: 'Read MCP/credential/broker/central/identity/Host-session readiness and safe nextActions. clientIssue reports a persistent local MCP block even after the provider recovers; repair as advised and reopen the Host. Unknown is not ready. This never starts a broker, changes settings, claims or binds a session, renews a lease, or changes work.',
    inputSchema: z.strictObject({}), outputSchema: connectionOutput,
    annotations: { readOnlyHint: true, destructiveHint: false, idempotentHint: true, openWorldHint: true },
    handler: (_args, ctx) => broker.getConnectionStatus({ signal: ctx.signal, mcpRequestId: ctx.mcpRequestId }),
  })];
}
