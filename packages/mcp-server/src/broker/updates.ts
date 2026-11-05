import { z } from 'zod';
import { decimalId } from './onboarding.js';

const eventId = z.string().min(1).max(128);
const version = z.string().regex(/^(0|[1-9][0-9]{0,18})$/)
  .refine(value => BigInt(value) <= 9_223_372_036_854_775_807n, 'Version must fit Java Long.');

/** Local advisory feed: IDs and states only. Prompts and results stay behind get_task_result. */
export const updateSchema = z.union([
  z.strictObject({ eventId, kind: z.literal('TASK_CREATED'), direction: z.literal('received'),
    taskId: decimalId, requesterAgentId: decimalId }),
  z.strictObject({ eventId, kind: z.literal('TASK_STATUS_CHANGED'), direction: z.literal('sent'),
    taskId: decimalId, taskVersion: version, status: z.enum(['IN_PROGRESS', 'FAILED', 'REJECTED']) }),
  z.strictObject({ eventId, kind: z.literal('TASK_COMPLETED'), direction: z.literal('sent'),
    taskId: decimalId, taskVersion: version, status: z.literal('COMPLETED') }),
]);
export const updatesSchema = z.strictObject({
  updates: z.array(updateSchema).max(50),
  nextCursor: z.string().regex(/^v1\.(0|[1-9][0-9]{0,9})\.(0|[1-9][0-9]{0,9})\.[0-9a-f]{32}$/),
  hasMore: z.boolean(),
  cursorReset: z.boolean(),
  coverage: z.literal('local'),
});
export const updatesOutput = z.strictObject({ requestId: z.string(),
  mcpRequestId: z.union([z.string(), z.number()]).optional(), data: updatesSchema });
