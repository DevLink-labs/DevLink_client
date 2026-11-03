export { principalSchema, sessionSchema, type BrokerSession } from './schemas.js';
export { BrokerFault, terminalSessionFaults, type FaultCode, type Correlation } from './faults.js';

/** MCP-side v0.1 contract; verify these routes against the Rust broker before integration. */
export const operationRoutes = {
  register_agent: '/v1/agents/register',
  list_agents: '/v1/agents/list',
  create_task: '/v1/tasks/create',
  update_task_status: '/v1/tasks/status',
  complete_task: '/v1/tasks/complete',
  get_tasks: '/v1/tasks/list',
  get_task_result: '/v1/tasks/result',
} as const;
export type BrokerOperation = keyof typeof operationRoutes;

export type BrokerResult = { requestId: string; mcpRequestId?: string | number; requestKey?: string; data: Record<string, unknown> };
