import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import { Client } from '@modelcontextprotocol/client';
import { InMemoryTransport } from '@modelcontextprotocol/server';
import { BrokerClient } from '../dist/broker/client.js';
import { operationRoutes } from '../dist/broker/contracts.js';
import { recommendationExamples, recommendationSchema } from '../dist/broker/recommendations.js';
import { createServer } from '../dist/server.js';
import { fakeBroker } from './helpers/fake-broker.mjs';

// S15P31D108-189 (FIND-3-02): find_agent result shape, per-state examples and Host guidance.
const silent = { debug() {}, info() {}, error() {} };
const examplesFile = new URL('../../../contracts/local-broker/v1/recommendation.examples.json', import.meta.url);

async function connected(t) {
  const broker = await fakeBroker();
  const adapter = new BrokerClient({ settings: broker.settings });
  const server = createServer(silent, adapter);
  const [serverTransport, clientTransport] = InMemoryTransport.createLinkedPair();
  const client = new Client({ name: 'recommendation-test', version: '1' }, { versionNegotiation: { mode: 'legacy' } });
  t.after(async () => { await client.close(); await adapter.close(); await server.close(); await broker.close(); });
  await Promise.all([server.connect(serverTransport), client.connect(clientTransport)]);
  assert.equal((await client.callTool({ name: 'register_agent', arguments: {} })).isError, undefined);
  return { broker, client };
}

test('the exported examples cover every state the Host explains and match the schema', async () => {
  const exported = JSON.parse(await readFile(examplesFile, 'utf8'));
  assert.deepEqual(exported, recommendationExamples);
  const states = Object.values(exported).map((e) => [e.status, e.insufficientReason, e.gitCoverage.state, e.gitCoverage.workingState]);
  assert.ok(states.some(([s, , g]) => s === 'MATCHED' && g === 'AVAILABLE'));
  assert.ok(states.some(([s, , g]) => s === 'MULTIPLE_CANDIDATES' && g === 'NOT_COLLECTED'));
  assert.ok(states.some(([, , , w]) => w === 'STALE'));
  assert.ok(states.some(([, r]) => r === 'NO_MATCHING_EVIDENCE'));
  assert.ok(states.some(([, r]) => r === 'NO_ELIGIBLE_AGENTS'));
  for (const example of Object.values(exported)) assert.equal(recommendationSchema.safeParse(example).success, true);
});

test('both Host guides explain every state and the score breakdown the same way', async () => {
  const guide = async (name) => (await readFile(new URL(`../docs/hosts/${name}.md`, import.meta.url), 'utf8'))
    .split(/\r?\n/).filter((line) => line.includes('find_agent')).join('\n');
  const [codex, claude] = await Promise.all([guide('codex'), guide('claude-code')]);
  assert.equal(codex, claude);
  for (const phrase of ['NO_ELIGIBLE_AGENTS', 'NO_MATCHING_EVIDENCE', 'NOT_COLLECTED', 'NO_TEAM_REPOSITORIES', 'NO_RETAINED_OBSERVATIONS',
    'STALE', 'workingState=NOT_COLLECTED', '60점', '40점', '점수에 들어가지 않음', 'repoId별', '둘 다 설명한다', '사용자가 고른 Agent ID로만 create_task']) {
    assert.ok(codex.includes(phrase), phrase);
  }
});

test('find_agent is read-only, explains each state and relays every example unchanged', async (t) => {
  const { broker, client } = await connected(t);
  const tool = (await client.listTools()).tools.find(({ name }) => name === 'find_agent');
  assert.equal(tool.annotations.readOnlyHint, true);
  assert.equal(tool.annotations.destructiveHint, false);
  for (const phrase of [/NO_ELIGIBLE_AGENTS/, /NO_MATCHING_EVIDENCE/, /NOT_COLLECTED/, /STALE/, /not part of the score/,
    /out of 60/, /out of 40/, /grouped by repoId/, /never send a task to a candidate the user did not choose/,
    /separate axis from INSUFFICIENT_EVIDENCE/, /explain both/, /zero-point reference candidates/, /workingState NOT_COLLECTED/]) {
    assert.match(tool.description, phrase);
  }
  assert.equal(tool.outputSchema.properties.data.properties.candidates.maxItems, 5);
  for (const [name, data] of Object.entries(recommendationExamples)) {
    broker.setBusinessHook(({ path, send }) => path === operationRoutes.find_agent && (send({ data }), true));
    const result = await client.callTool({ name: 'find_agent', arguments: { question: '로그인 오류' } });
    assert.equal(result.isError, undefined, name);
    assert.deepEqual(result.structuredContent.data, data, name);
  }
});

test('results that break the contract or leak extra fields become protocol errors', async (t) => {
  const { broker, client } = await connected(t);
  const base = recommendationExamples.matchedWithGit;
  const changed = (edit) => { const copy = structuredClone(base); edit(copy); return copy; };
  const candidate = base.candidates[0];
  for (const data of [
    changed((d) => { d.candidates[0].credential = 'secret token value'; }),
    changed((d) => { d.prompt = 'secret token value'; }),
    changed((d) => { d.candidates.push({ ...candidate, agentId: '14' }); }),
    changed((d) => { d.status = 'MULTIPLE_CANDIDATES'; }),
    changed((d) => { d.candidates.push({ ...candidate }); d.status = 'MULTIPLE_CANDIDATES'; }),
    changed((d) => { d.insufficientReason = 'NO_MATCHING_EVIDENCE'; }),
    changed((d) => { d.status = 'INSUFFICIENT_EVIDENCE'; d.insufficientReason = 'NO_ELIGIBLE_AGENTS'; }),
    changed((d) => { d.status = 'INSUFFICIENT_EVIDENCE'; d.insufficientReason = 'NO_MATCHING_EVIDENCE'; d.candidates[0].insufficientReason = 'NO_MATCHING_EVIDENCE'; }),
    changed((d) => { d.candidates[0].score = 0; }),
    changed((d) => { d.candidates[0].scores.role = 80; }),
    changed((d) => { d.candidates[0].scores.git = 41; }),
    changed((d) => { d.candidates[0].connectionStatus = 'AWAY'; }),
    changed((d) => { d.candidates[0].agentId = 13; }),
    changed((d) => { d.candidates[0].agentId = '9223372036854775808'; }),
    changed((d) => { d.candidates[0].relatedFiles[0].sources = ['DEPENDENCY']; }),
    changed((d) => { d.gitCoverage.reason = 'NO_TEAM_REPOSITORIES'; }),
    changed((d) => { d.gitCoverage.state = 'NOT_COLLECTED'; }),
    changed((d) => { d.evaluatedAt = 'yesterday'; }),
    changed((d) => { d.candidates = Array.from({ length: 6 }, (_, i) => ({ ...candidate, agentId: String(13 + i) })); d.status = 'MULTIPLE_CANDIDATES'; }),
  ]) {
    broker.setBusinessHook(({ send }) => (send({ data }), true));
    const result = await client.callTool({ name: 'find_agent', arguments: { question: '로그인' } });
    assert.equal(result.isError, true, JSON.stringify(data).slice(0, 200));
    assert.equal(result.structuredContent.error.code, 'BROKER_PROTOCOL_ERROR');
    assert.ok(!JSON.stringify(result).includes('secret token value'));
  }
});
