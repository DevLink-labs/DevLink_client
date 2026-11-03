import { request } from 'node:http';
import { realpathSync } from 'node:fs';
import { parseArgs } from 'node:util';
import { pathToFileURL } from 'node:url';
import { readBrokerSettings, validateSettings } from '../dist/broker/settings.js';
import { operationRoutes } from '../dist/broker/contracts.js';

// OPTIONS never claims a session, binds an Agent, or changes a task. A response
// only describes this probe; middleware can mask routes, so none proves readiness.
const sessionPath = '/v1/sessions/00000000-0000-4000-8000-000000000066';
export const requiredRoutes = Object.freeze([
  '/v1/sessions', `${sessionPath}/bind`, `${sessionPath}/heartbeat`, sessionPath,
  ...Object.values(operationRoutes),
]);
const centralStates = ['Connecting', 'Authenticating', 'Connected', 'Reconnecting', 'AuthFailed', 'Disconnected'];

function probe(settings, path, method, timeoutMs) {
  return new Promise((resolve) => {
    let settled = false;
    let timer;
    const finish = (result) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      resolve(result);
    };
    // No redirects; agent:false avoids the global agent (including env proxies).
    const req = request(new URL(path, settings.brokerUrl), {
      method, agent: false, headers: { Authorization: `Bearer ${settings.localToken}` },
    }, (res) => {
      let size = 0;
      const chunks = [];
      res.on('data', (chunk) => {
        size += chunk.length;
        if (size > 16_384) {
          finish({ outcome: 'RESPONSE_TOO_LARGE', httpStatus: res.statusCode });
          res.destroy();
        } else if (method === 'GET') chunks.push(chunk);
      });
      res.on('error', () => finish({ outcome: 'CONNECTION_CLOSED' }));
      res.on('end', () => {
        let health;
        if (method === 'GET' && res.statusCode === 200) {
          try {
            const data = JSON.parse(Buffer.concat(chunks).toString('utf8'));
            if (data.service === 'devlink-broker' && data.apiVersion === 1
              && Number.isInteger(data.pid) && data.pid > 0
              && typeof data.instanceId === 'string' && data.instanceId.length > 0
              && centralStates.includes(data.centralServer)) {
              // Never copy raw response fields (including unexpected secrets).
              health = { centralServer: data.centralServer };
            }
          } catch { /* Invalid health is reported without echoing the body. */ }
        }
        finish({ outcome: 'HTTP_RESPONSE', httpStatus: res.statusCode, ...(health ? { health } : {}) });
      });
    });
    req.on('error', () => finish({ outcome: 'UNREACHABLE' }));
    timer = setTimeout(() => {
      finish({ outcome: 'TIMEOUT' });
      req.destroy();
    }, timeoutMs);
    req.end();
  });
}

export async function inspectBroker(value, timeoutMs = 2000) {
  const settings = validateSettings(value);
  if (!Number.isInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > 10_000) throw new Error('INVALID_TIMEOUT');
  const report = {
    schemaVersion: 1, story: 'S15P31D108-66', checkedAt: new Date().toISOString(),
    status: 'blocked', actualMvpVerified: false, credentials: 'valid', blockers: [], routes: [],
  };
  const health = await probe(settings, '/health', 'GET', timeoutMs);
  report.health = health;
  if (!health.health) {
    report.blockers.push('BROKER_HEALTH_UNVERIFIED');
    return report; // Never send credentials to more endpoints of an unidentified service.
  }
  if (health.health.centralServer !== 'Connected') report.blockers.push('CENTRAL_NOT_CONNECTED');
  for (const path of requiredRoutes) {
    const result = await probe(settings, path, 'OPTIONS', timeoutMs);
    // 404 can also come from a proxy/middleware: it blocks this run, but is not
    // by itself proof that the corresponding POST/DELETE implementation is absent.
    const observation = result.httpStatus === 404 ? 'NOT_FOUND'
      : [401, 403].includes(result.httpStatus) ? 'AUTH_REJECTED' : 'INCONCLUSIVE';
    report.routes.push({ path, method: 'OPTIONS', observation, ...result });
    if (observation === 'NOT_FOUND') report.blockers.push('REQUIRED_ROUTE_NOT_FOUND');
    if (observation === 'AUTH_REJECTED') report.blockers.push('ROUTE_AUTH_REJECTED');
    if (result.outcome !== 'HTTP_RESPONSE' || (result.httpStatus >= 300 && result.httpStatus !== 405)) {
      report.blockers.push('ROUTE_PROBE_UNVERIFIED');
    }
  }
  report.blockers = [...new Set(report.blockers)];
  report.status = report.blockers.length ? 'blocked' : 'unverified';
  return report;
}

export async function checkIntegration(settingsPath, timeoutMs = 2000) {
  let settings;
  try { settings = await readBrokerSettings(settingsPath ?? ''); }
  catch {
    return { schemaVersion: 1, story: 'S15P31D108-66', checkedAt: new Date().toISOString(),
      status: 'blocked', actualMvpVerified: false, credentials: 'invalid',
      blockers: ['CREDENTIAL_FILE_INVALID'], routes: [] };
  }
  return inspectBroker(settings, timeoutMs);
}

async function main() {
  try {
    const { values } = parseArgs({ options: {
      'broker-file': { type: 'string' }, 'timeout-ms': { type: 'string' }, help: { type: 'boolean' },
    }, allowPositionals: false });
    if (values.help) {
      console.log('node scripts/integration-readiness.mjs [--broker-file ABSOLUTE_PATH] [--timeout-ms 2000]');
      console.log('Defaults to DEVLINK_BROKER_FILE. Exit 2=blocked, 3=unverified; never certifies MVP.');
      return;
    }
    const timeout = values['timeout-ms'] ?? '2000';
    if (!/^[1-9][0-9]*$/.test(timeout) || Number(timeout) > 10_000) throw new Error('INVALID_TIMEOUT');
    const report = await checkIntegration(values['broker-file'] ?? process.env.DEVLINK_BROKER_FILE, Number(timeout));
    console.log(JSON.stringify(report, null, 2));
    process.exitCode = report.status === 'blocked' ? 2 : 3;
  } catch {
    console.log(JSON.stringify({ error: 'INVALID_ARGUMENTS', actualMvpVerified: false }));
    process.exitCode = 1;
  }
}
if (process.argv[1] && import.meta.url === pathToFileURL(realpathSync(process.argv[1])).href) await main();
