import { z } from 'zod';
import { identifierSchema } from './schemas.js';

export const remoteErrorSchema = z.object({
  code: z.string().min(1).max(128),
  scope: z.enum(['local_auth', 'session', 'domain', 'transport']).optional(),
  outcome: z.enum(['unknown', 'rejected']).optional(),
  taskId: identifierSchema.optional(), requestKey: identifierSchema.optional(),
});
export const brokerEnvelopeSchema = z.union([
  z.strictObject({ requestId: identifierSchema, requestKey: identifierSchema.optional(), data: z.record(z.string(), z.unknown()) }),
  z.strictObject({ requestId: identifierSchema, requestKey: identifierSchema.optional(), error: remoteErrorSchema }),
]);
export type RemoteError = z.infer<typeof remoteErrorSchema>;
export type BrokerResponse = { data: Record<string, unknown>; requestKey?: string };
