import { z } from 'zod';
import { decimalId } from './onboarding.js';

const part = z.enum(['FRONTEND', 'BACKEND', 'INFRA', 'OTHER']);
const version = z.string().regex(/^(0|[1-9][0-9]{0,18})$/);
// Summary entries are text the central server derived from task questions and answers. Their
// fields follow the central prompt version, so they are relayed as objects without being read.
const entries = z.array(z.record(z.string(), z.unknown()));

/**
 * S15P31D108-211: the latest server-maintained summary between this Agent's part and its
 * counterparts. Without a requested counterpart only pairs with a stored summary are listed
 * (possibly none); a requested pair without one has version "0" and empty lists.
 */
export const teamContextSchema = z.strictObject({
  teamId: decimalId,
  part,
  contexts: z.array(z.strictObject({
    counterpart: part,
    version,
    lastEventId: decimalId.nullable(),
    updatedAt: z.string().nullable(),
    model: z.string().nullable(),
    promptVersion: z.string().nullable(),
    common: entries,
    changed: entries,
    issues: entries,
  })).max(4),
});
export const teamContextOutput = z.strictObject({ requestId: z.string(),
  mcpRequestId: z.union([z.string(), z.number()]).optional(), data: teamContextSchema });
export type TeamContext = z.input<typeof teamContextSchema>;
