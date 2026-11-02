import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { z } from 'zod';
import { credentialSchema, principalSchema, sessionSchema } from '../dist/broker/schemas.js';
import { brokerEnvelopeSchema } from '../dist/broker/envelopes.js';
import { contextSchema } from '../dist/broker/onboarding.js';

const destination = new URL('../../../contracts/local-broker/v1/', import.meta.url);
const check = process.argv.includes('--check');
if (!check) await mkdir(destination, { recursive: true });
for (const [name, schema] of Object.entries({ credentials: credentialSchema, principal: principalSchema, session: sessionSchema, envelope: brokerEnvelopeSchema, context: contextSchema })) {
  const value = { $id: `urn:devlink:local-broker:v1:${name}`, ...z.toJSONSchema(schema, { io: 'input' }) };
  const expected = `${JSON.stringify(value, null, 2)}\n`;
  const file = new URL(`${name}.schema.json`, destination);
  if (check) {
    if ((await readFile(file, 'utf8')).replace(/\r\n/g, '\n') !== expected) throw new Error(`Contract drift: ${name}.schema.json. Run npm run contracts:write.`);
  } else await writeFile(file, expected);
}
