import { randomBytes } from 'node:crypto';
import { fakeBroker } from './fake-broker.mjs';

/** Synthetic central state only. No Rust or real central API is implemented by this fixture. */
export async function hostFixture() {
  const profiles = Object.fromEntries(['codex', 'claude'].map((host) => [host, {
    localToken: randomBytes(32).toString('base64url'), userId: `user-64-${host}`, teamId: 'team-64-fixture',
    agentId: `agent-64-${host}`, toolType: host === 'codex' ? 'codex' : 'claude_code',
  }]));
  const broker = await fakeBroker({ profiles: Object.values(profiles) });
  const tasks = Object.fromEntries(['codex', 'claude'].map((host) => [host, { taskId: `task-64-${host}`, status: 'PENDING',
    recipientAgentId: 'recipient-64', requestKey: `create-key-${host}`, delivery: { status: 'UNKNOWN' } }]));
  broker.setBusinessHook(async ({ body, res, send, profile }) => {
    const host = profile.toolType === 'codex' ? 'codex' : 'claude';
    const task = tasks[host];
    const operation = body.operation;
    if (!operation) return false;
    if (operation === 'get_task_result') {
      if (body.arguments.taskId === 'forbidden-64') { send({ error: { scope: 'domain', code: 'FORBIDDEN' } }, 403); return true; }
      if (body.arguments.taskId === 'slow-64') return true;
      if (body.arguments.taskId === 'restart-64') { broker.restart(); return true; }
      if (body.arguments.taskId === 'auth-64') { res.writeHead(401).end(`private-${profile.localToken}`); return true; }
      send({ requestKey: `read-key-${host}`, data: task }); return true;
    }
    if (operation === 'register_agent') { send({ data: { agentId: profile.agentId } }); return true; }
    if (operation === 'list_agents') { send({ data: { agents: [{ agentId: 'recipient-64', name: 'Synthetic recipient' }] } }); return true; }
    if (operation === 'create_task') { task.status = 'PENDING'; send({ requestKey: task.requestKey, data: task }); return true; }
    if (operation === 'get_tasks') { send({ data: { tasks: [task] } }); return true; }
    if (operation === 'update_task_status') { task.status = body.arguments.status; send({ data: task }); return true; }
    if (operation === 'complete_task') { task.status = 'COMPLETED'; send({ data: task }); return true; }
    return false;
  });
  return {
    broker, profile: (host) => profiles[host], task: (host) => tasks[host],
    rotate(host) {
      const previous = profiles[host];
      broker.revokeToken(previous.localToken);
      profiles[host] = { ...previous, localToken: randomBytes(32).toString('base64url') };
      broker.addProfile(profiles[host]);
      return profiles[host];
    },
  };
}

export function matrixSteps(host) {
  const toolType = host === 'codex' ? 'codex' : 'claude_code';
  const taskId = `task-64-${host}`;
  return [
    { tool: 'register_agent', args: { name: 'compatibility fixture', toolType }, expect: 'success' },
    { tool: 'list_agents', args: {}, expect: 'success' },
    { tool: 'create_task', args: { recipientAgentId: 'recipient-64', title: 'Synthetic task', description: 'Compatibility check only' }, expect: 'PENDING' },
    { tool: 'get_tasks', args: {}, expect: 'success' },
    { tool: 'get_task_result', args: { taskId }, expect: 'PENDING' },
    { tool: 'update_task_status', args: { taskId, status: 'FAILED' }, expect: 'input-validation' },
    { tool: 'update_task_status', args: { taskId, version: '0', status: 'IN_PROGRESS' }, expect: 'success' },
    { tool: 'complete_task', args: { taskId, version: '1', resultText: 'Synthetic completion; no local files changed', taskFiles: [] }, expect: 'success' },
    { tool: 'get_task_result', args: { taskId: 'forbidden-64' }, expect: 'BROKER_FORBIDDEN' },
    { tool: 'get_task_result', args: { taskId }, expect: 'success' },
    { tool: 'get_task_result', args: { taskId: 'slow-64' }, expect: 'BROKER_TIMEOUT' },
    { tool: 'get_task_result', args: { taskId }, expect: 'success' },
    { tool: 'get_task_result', args: { taskId: 'restart-64' }, expect: 'BROKER_CONNECTION_CLOSED' },
    { tool: 'get_task_result', args: { taskId }, expect: 'success' },
    { tool: 'get_task_result', args: { taskId: 'auth-64' }, expect: 'BROKER_UNAUTHORIZED' },
    { tool: 'get_task_result', args: { taskId }, expect: 'BROKER_UNAUTHORIZED' },
  ];
}
