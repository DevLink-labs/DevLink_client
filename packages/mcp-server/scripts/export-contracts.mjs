import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { z } from 'zod';
import { credentialSchema, principalSchema, sessionSchema } from '../dist/broker/schemas.js';
import { brokerEnvelopeSchema } from '../dist/broker/envelopes.js';
import { contextSchema, snapshotSchema, connectionSchema } from '../dist/broker/onboarding.js';
import { updatesSchema } from '../dist/broker/updates.js';
import { recommendationExamples, recommendationSchema } from '../dist/broker/recommendations.js';

const destination = new URL('../../../contracts/local-broker/v1/', import.meta.url);
const check = process.argv.includes('--check');
if (!check) await mkdir(destination, { recursive: true });
for (const [name, schema] of Object.entries({ credentials: credentialSchema, principal: principalSchema, session: sessionSchema, envelope: brokerEnvelopeSchema, context: contextSchema, snapshot: snapshotSchema, connection: connectionSchema, updates: updatesSchema, recommendation: recommendationSchema })) {
  const value = { $id: `urn:devlink:local-broker:v1:${name}`, ...z.toJSONSchema(schema, { io: 'input' }) };
  const expected = `${JSON.stringify(value, null, 2)}\n`;
  const file = new URL(`${name}.schema.json`, destination);
  if (check) {
    if ((await readFile(file, 'utf8')).replace(/\r\n/g, '\n') !== expected) throw new Error(`Contract drift: ${name}.schema.json. Run npm run contracts:write.`);
  } else await writeFile(file, expected);
}
// S15P31D108-189: one find_agent result per state, valid against the recommendation schema.
for (const example of Object.values(recommendationExamples)) recommendationSchema.parse(example);
const examples = `${JSON.stringify(recommendationExamples, null, 2)}\n`;
const examplesFile = new URL('recommendation.examples.json', destination);
if (check) {
  if ((await readFile(examplesFile, 'utf8')).replace(/\r\n/g, '\n') !== examples) throw new Error('Contract drift: recommendation.examples.json. Run npm run contracts:write.');
} else await writeFile(examplesFile, examples);
