import { z } from 'zod';
import { toolTypeSchema } from './schemas.js';
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
