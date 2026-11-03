import { z } from 'zod';

// Identifiers are opaque: reject malformed values rather than silently trimming them.
// Spell out whitespace/control ranges: JS and Rust's Unicode \s sets differ.
export const identifierSchema = z.string().min(1).regex(/^[^\x00-\x20\x7f-\x9f\u00a0\u1680\u2000-\u200a\u2028\u2029\u202f\u205f\u3000\ufeff]+$/)
  .refine((value) => [...value].length <= 128, 'Identifier exceeds 128 code points')
  .meta({ maxLength: 128 });
export const toolTypeSchema = z.enum(['codex', 'claude_code']);
export const credentialSchema = z.strictObject({
  schemaVersion: z.literal(1),
  brokerUrl: z.string().url().regex(/^http:\/\/127\.0\.0\.1:(?:[1-9][0-9]{0,3}|[1-5][0-9]{4}|6[0-4][0-9]{3}|65[0-4][0-9]{2}|655[0-2][0-9]|6553[0-5])\/?$/),
  localToken: z.string().min(16).max(4096).regex(/^[A-Za-z0-9._~-]+$/),
  agentId: identifierSchema.optional(),
});
export const principalSchema = z.strictObject({
  userId: identifierSchema,
  teamId: identifierSchema,
  agentId: identifierSchema.optional(),
  // v0.1 brokers may omit this. Never infer it from register_agent arguments.
  toolType: toolTypeSchema.optional(),
});
export const sessionSchema = z.strictObject({ sessionId: identifierSchema, principal: principalSchema });
export type BrokerSession = Readonly<{ sessionId: string; principal: Readonly<z.infer<typeof principalSchema>> }>;

export function immutableSession(value: z.infer<typeof sessionSchema>): BrokerSession {
  return Object.freeze({ sessionId: value.sessionId, principal: Object.freeze({ ...value.principal }) });
}
