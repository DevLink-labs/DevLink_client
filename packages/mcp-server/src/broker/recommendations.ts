import { z } from 'zod';

// S15P31D108-189 (FIND-3-02): the find_agent result as the broker relays it from the central
// recommendation (server docs/agent-recommendation.md). Scores are computed centrally; this
// schema fixes the shape and the documented bounds (role 60, Git 40 = working 15 + frequency 15
// + recency 10). connectionStatus is display-only and never part of the score.

// Zod still runs a refinement after a failed regex, so guard BigInt against non-digits.
const javaLong = (value: string) => /^[0-9]+$/.test(value) && BigInt(value) <= 9_223_372_036_854_775_807n;
const id = z.string().regex(/^[1-9][0-9]{0,18}$/).refine(javaLong, 'ID must fit Java Long.');
const instant = z.iso.datetime({ offset: true });
const points = (max: number) => z.number().min(0).max(max);

export const insufficientReasonSchema = z.enum(['NO_ELIGIBLE_AGENTS', 'NO_MATCHING_EVIDENCE']);

export const recommendationCandidateSchema = z.strictObject({
  agentId: id,
  name: z.string().min(1),
  // FRONTEND, BACKEND, INFRA or OTHER today; kept open for new central parts.
  part: z.string().min(1).nullable(),
  connectionStatus: z.enum(['ONLINE', 'OFFLINE']),
  score: points(100),
  scores: z.strictObject({
    role: points(60), git: points(40), working: points(15), frequency: points(15), recency: points(10),
  }),
  matchedRoles: z.array(z.string().min(1)),
  relatedFiles: z.array(z.strictObject({
    repoId: id,
    relativePath: z.string().min(1),
    // WORKING and/or COMMIT; the broker drops source values it does not know.
    sources: z.array(z.enum(['WORKING', 'COMMIT'])),
    observedAt: instant.nullable(),
  })).max(5),
  latestCommitAt: instant.nullable(),
  workingObservedAt: instant.nullable(),
  insufficientReason: z.literal('NO_MATCHING_EVIDENCE').nullable(),
});

export const recommendationSchema = z.strictObject({
  status: z.enum(['MATCHED', 'MULTIPLE_CANDIDATES', 'INSUFFICIENT_EVIDENCE']),
  evaluatedAt: instant,
  insufficientReason: insufficientReasonSchema.nullable(),
  gitCoverage: z.strictObject({
    state: z.enum(['AVAILABLE', 'NOT_COLLECTED']),
    reason: z.enum(['NO_TEAM_REPOSITORIES', 'NO_RETAINED_OBSERVATIONS']).nullable(),
    workingState: z.enum(['FRESH', 'STALE', 'NOT_COLLECTED']),
  }),
  candidates: z.array(recommendationCandidateSchema).max(5),
}).superRefine((data, ctx) => {
  const { status, insufficientReason: reason, candidates } = data;
  const fail = (message: string) => ctx.addIssue({ code: 'custom', message });
  if ((data.gitCoverage.state === 'AVAILABLE') !== (data.gitCoverage.reason === null)) {
    fail('gitCoverage.reason is set exactly when state is NOT_COLLECTED.');
  }
  if (new Set(candidates.map(c => c.agentId)).size !== candidates.length) fail('Candidate IDs must be unique.');
  const consistent = status === 'MATCHED' ? reason === null && candidates.length === 1
    : status === 'MULTIPLE_CANDIDATES' ? reason === null && candidates.length >= 2
    : reason === 'NO_ELIGIBLE_AGENTS' ? candidates.length === 0
    : reason === 'NO_MATCHING_EVIDENCE' && candidates.length >= 1;
  if (!consistent) fail('status, insufficientReason and candidate count disagree.');
  const candidateReason = status === 'INSUFFICIENT_EVIDENCE' ? 'NO_MATCHING_EVIDENCE' : null;
  if (candidates.some(c => c.insufficientReason !== candidateReason)) fail('Candidate insufficientReason disagrees with status.');
  // Matched candidates have a positive score; reference candidates have none.
  if (candidates.some(c => (candidateReason === null) !== (c.score > 0))) fail('Candidate score disagrees with status.');
});

export const recommendationOutput = z.strictObject({ requestId: z.string(),
  mcpRequestId: z.union([z.string(), z.number()]).optional(), data: recommendationSchema });

export type Recommendation = z.input<typeof recommendationSchema>;

const evaluatedAt = '2026-11-09T02:00:00Z';
const noGit = { state: 'NOT_COLLECTED', reason: 'NO_TEAM_REPOSITORIES', workingState: 'NOT_COLLECTED' } as const;
const zero = { role: 0, git: 0, working: 0, frequency: 0, recency: 0 };

/**
 * One example per state the Host must explain differently. Exported to
 * contracts/local-broker/v1/recommendation.examples.json and checked against the schema.
 */
export const recommendationExamples: Record<string, Recommendation> = {
  matchedWithGit: {
    status: 'MATCHED', evaluatedAt, insufficientReason: null,
    gitCoverage: { state: 'AVAILABLE', reason: null, workingState: 'FRESH' },
    candidates: [{
      agentId: '13', name: '로그인 담당 Agent', part: 'BACKEND', connectionStatus: 'ONLINE', score: 51.92,
      // recency = 10 × 2^(-4.7 days / 14), rounded HALF_UP to 2 decimals as the server does.
      scores: { role: 20, git: 31.92, working: 15, frequency: 9, recency: 7.92 },
      matchedRoles: ['로그인'],
      relatedFiles: [
        { repoId: '4', relativePath: 'src/auth/LoginService.java', sources: ['WORKING', 'COMMIT'], observedAt: '2026-11-09T01:59:30Z' },
        { repoId: '4', relativePath: 'src/auth/LoginController.java', sources: ['COMMIT'], observedAt: '2026-11-04T09:12:00Z' },
      ],
      latestCommitAt: '2026-11-04T09:12:00Z', workingObservedAt: '2026-11-09T01:59:30Z', insufficientReason: null,
    }],
  },
  multipleRoleOnly: {
    status: 'MULTIPLE_CANDIDATES', evaluatedAt, insufficientReason: null, gitCoverage: noGit,
    candidates: [
      { agentId: '13', name: '로그인 담당 Agent', part: 'BACKEND', connectionStatus: 'ONLINE', score: 20,
        scores: { ...zero, role: 20 }, matchedRoles: ['로그인'], relatedFiles: [],
        latestCommitAt: null, workingObservedAt: null, insufficientReason: null },
      { agentId: '21', name: '인증 화면 Agent', part: 'FRONTEND', connectionStatus: 'OFFLINE', score: 20,
        scores: { ...zero, role: 20 }, matchedRoles: ['로그인 화면'], relatedFiles: [],
        latestCommitAt: null, workingObservedAt: null, insufficientReason: null },
    ],
  },
  staleWorkingFiles: {
    status: 'MATCHED', evaluatedAt, insufficientReason: null,
    gitCoverage: { state: 'AVAILABLE', reason: null, workingState: 'STALE' },
    candidates: [{
      agentId: '13', name: '로그인 담당 Agent', part: 'BACKEND', connectionStatus: 'ONLINE', score: 9.72,
      // recency = 10 × 2^(-19.96 days / 14) = 3.72.
      scores: { role: 0, git: 9.72, working: 0, frequency: 6, recency: 3.72 },
      matchedRoles: [],
      relatedFiles: [{ repoId: '4', relativePath: 'src/auth/LoginService.java', sources: ['COMMIT'], observedAt: '2026-10-20T03:00:00Z' }],
      latestCommitAt: '2026-10-20T03:00:00Z', workingObservedAt: null, insufficientReason: null,
    }],
  },
  noMatchingEvidence: {
    status: 'INSUFFICIENT_EVIDENCE', evaluatedAt, insufficientReason: 'NO_MATCHING_EVIDENCE',
    gitCoverage: { state: 'NOT_COLLECTED', reason: 'NO_RETAINED_OBSERVATIONS', workingState: 'NOT_COLLECTED' },
    candidates: [{
      agentId: '13', name: '로그인 담당 Agent', part: 'BACKEND', connectionStatus: 'OFFLINE', score: 0,
      scores: zero, matchedRoles: [], relatedFiles: [],
      latestCommitAt: null, workingObservedAt: null, insufficientReason: 'NO_MATCHING_EVIDENCE',
    }],
  },
  noEligibleAgents: {
    status: 'INSUFFICIENT_EVIDENCE', evaluatedAt, insufficientReason: 'NO_ELIGIBLE_AGENTS', gitCoverage: noGit,
    candidates: [],
  },
};
