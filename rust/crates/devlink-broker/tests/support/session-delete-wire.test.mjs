import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import test from 'node:test';
import { BrokerHttp } from '../../../../../packages/mcp-server/dist/broker/http.js';

test('session DELETE transmits the complete UTF-8 requestId and instanceId body', async () => {
  const received = [];
  const server = createServer(async (req, res) => {
    const chunks = [];
    for await (const chunk of req) chunks.push(chunk);
    const bytes = Buffer.concat(chunks);
    const body = bytes.length ? JSON.parse(bytes.toString('utf8')) : {};
    received.push({ method: req.method, body, length: req.headers['content-length'], bytes: bytes.length });
    res.setHeader('content-type', 'application/json');
    res.end(JSON.stringify({ requestId: body.requestId ?? 'missing', data: {} }));
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const http = new BrokerHttp({ schemaVersion: 1,
    brokerUrl: `http://127.0.0.1:${server.address().port}`, localToken: 'synthetic-local-token',
  }, 2000);
  const body = { requestId: '해제-요청', instanceId: '인스턴스-1' };
  try {
    http.close();
    await http.request('/v1/sessions/session-1', body, { method: 'DELETE', duringClose: true });
    assert.equal(received.length, 1);
    assert.equal(received[0].method, 'DELETE');
    assert.deepEqual(received[0].body, body);
    assert.equal(Number(received[0].length), Buffer.byteLength(JSON.stringify(body)));
    assert.equal(received[0].bytes, Number(received[0].length));
  } finally {
    http.close();
    await new Promise(resolve => server.close(resolve));
  }
});
