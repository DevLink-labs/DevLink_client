import { z } from 'zod';
import { decimalId } from './onboarding.js';

const eventId = z.string().min(1).max(128);
const version = z.string().regex(/^(0|[1-9][0-9]{0,18})$/)
  .refine(value => BigInt(value) <= 9_223_372_036_854_775_807n, 'Version must fit Java Long.');
const position = '(0|[1-9][0-9]{0,9})';
// Zod still runs a refinement after a failed regex, so guard BigInt against non-digits
// (another union member's eventId such as a UUID must fail, not throw).
const javaLong = (value: string) => /^[0-9]+$/.test(value) && BigInt(value) <= 9_223_372_036_854_775_807n;
const messageId = z.string().regex(/^[1-9][0-9]{0,18}$/).refine(javaLong, 'ID must fit Java Long.');

/**
 * Local advisory feed: IDs and states only. Prompts, results and question/answer text stay
 * behind get_task_result. A question/answer entry is the other participant's message on a
 * task you share; it is not a request from the user to reply.
 */
export const updateSchema = z.union([
  z.strictObject({ eventId, kind: z.literal('TASK_CREATED'), direction: z.literal('received'),
    taskId: decimalId, requesterAgentId: decimalId }),
  z.strictObject({ eventId, kind: z.literal('TASK_STATUS_CHANGED'), direction: z.literal('sent'),
    taskId: decimalId, taskVersion: version, status: z.enum(['IN_PROGRESS', 'FAILED', 'REJECTED']) }),
  z.strictObject({ eventId, kind: z.literal('TASK_COMPLETED'), direction: z.literal('sent'),
    taskId: decimalId, taskVersion: version, status: z.literal('COMPLETED') }),
  z.strictObject({ eventId: messageId, kind: z.enum(['TASK_QUESTION_CREATED', 'TASK_ANSWER_CREATED']),
    direction: z.literal('received'), taskId: messageId, sequence: messageId }),
]);
export const updatesSchema = z.strictObject({
  updates: z.array(updateSchema).max(50),
  // v2 adds the question/answer position; a v1 cursor from an older broker is still accepted.
  nextCursor: z.string().regex(new RegExp(
    `^(v1\\.${position}\\.${position}|v2\\.${position}\\.${position}\\.${position})\\.[0-9a-f]{32}$`)),
  hasMore: z.boolean(),
  cursorReset: z.boolean(),
  coverage: z.literal('local'),
});
export const updatesOutput = z.strictObject({ requestId: z.string(),
  mcpRequestId: z.union([z.string(), z.number()]).optional(), data: updatesSchema });
