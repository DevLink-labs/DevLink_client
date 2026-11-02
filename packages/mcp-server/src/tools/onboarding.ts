import { z } from 'zod';
import type { BrokerClient } from '../broker/client.js';
import { contextOutput } from '../broker/onboarding.js';
import { defineTool } from './registry.js';
export function onboardingTools(broker: Pick<BrokerClient, 'getMyContext'>) {
  return [defineTool({ name: 'get_my_context',
    description: 'Read verified user/team/agent and project provenance without claiming, binding or renewing a session. Unavailable fields are not inferred. Reconfirm the CLI profile after a context change.',
    inputSchema: z.strictObject({}), outputSchema: contextOutput,
    annotations: { readOnlyHint: true, destructiveHint: false, idempotentHint: true, openWorldHint: true },
    handler: (_args, ctx) => broker.getMyContext({ signal: ctx.signal, mcpRequestId: ctx.mcpRequestId }),
  })];
}
