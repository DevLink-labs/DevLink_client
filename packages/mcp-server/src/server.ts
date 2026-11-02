import { McpServer } from '@modelcontextprotocol/server';
import type { Logger } from './logger.js';
import type { BrokerClient } from './broker/client.js';
import { businessTools } from './tools/business.js';
import { pingTool } from './tools/ping.js';
import { onboardingTools } from './tools/onboarding.js';
import { registerTools, type RegisteredDefinition } from './tools/registry.js';

export const SERVER_INFO = Object.freeze({ name: 'devlink-mcp', version: '0.1.0' });

export function createServer(logger: Logger, broker: Pick<BrokerClient, 'execute' | 'getMyContext'>, extensions: readonly RegisteredDefinition[] = []): McpServer {
  const server = new McpServer(SERVER_INFO);
  registerTools(server, [pingTool(logger, SERVER_INFO.version), ...businessTools(broker), ...onboardingTools(broker), ...extensions]);
  return server;
}
