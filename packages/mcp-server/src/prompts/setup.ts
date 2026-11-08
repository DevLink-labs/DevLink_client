import type { McpServer } from '@modelcontextprotocol/server';
import type { z } from 'zod';
import type { BrokerClient } from '../broker/client.js';
import { connectionSchema, type nextAction } from '../broker/onboarding.js';

type NextAction = z.infer<typeof nextAction>;
type Connection = z.infer<typeof connectionSchema>;
export const SETUP_PROMPT_NAME = 'devlink_setup';

/** Fixed advice per diagnostic action. `user` steps are run by the person, never by the assistant. */
export const setupGuidance: Readonly<Record<NextAction, { actor: 'user' | 'assistant_on_request'; text: string }>> = Object.freeze({
  prepare_cli_profile: { actor: 'user', text: 'No DevLink broker profile is available to this MCP server. In your own terminal run `devlink setup` (it handles login, team, project, tool and Agent selection), then reopen this Host.' },
  repair_private_file: { actor: 'user', text: 'The broker credential file was rejected or cannot be used. Check it with `devlink setup status` and repair it with the DevLink CLI; never paste its contents into chat. Then reopen this Host.' },
  inspect_local_broker: { actor: 'user', text: 'The local DevLink broker did not answer. Check it with `devlink status` and start it with `devlink start` if it is stopped.' },
  check_network_then_retry: { actor: 'user', text: 'The broker cannot reach the DevLink central service. Check the network or central availability, then run this prompt again.' },
  stop_then_reauthenticate_agent: { actor: 'user', text: 'Central rejected this Agent credential. In your own terminal run `devlink stop`, then `devlink agent token` to issue a new Agent token, then `devlink start`, and reopen this Host. Issuing a token is a deliberate step only you run; it is never done from chat.' },
  inspect_profile_then_rebind: { actor: 'user', text: 'The verified identity or project differs from what this MCP process saw. Inspect it with `devlink setup status`. If it shows the Agent and project you intend, just reopen this Host. Otherwise run `devlink setup` again, or `devlink stop`, `devlink agent use --id <ID> --project <PATH>`, `devlink agent token`, `devlink start` (rebinding needs a stopped broker and a new Agent token), then reopen this Host.' },
  inspect_connection_owner: { actor: 'user', text: 'Another MCP process owns this Agent session. Check whether that process is yours and still running. Only after it has stopped, wait for its lease (about 90 seconds) to expire, then reopen this Host. Do not force a takeover.' },
  check_provider_contract: { actor: 'user', text: 'The broker and this MCP server disagree on the protocol. Update DevLink so the broker and MCP server come from the same release.' },
  repair_inbox_then_restart: { actor: 'user', text: 'The broker task inbox failed. Check `devlink status`, fix the reported inbox problem, then restart the broker with `devlink stop` and `devlink start`.' },
  register_agent: { actor: 'assistant_on_request', text: 'Bind this Host session to your existing DevLink Agent. On request the assistant first calls get_my_context({}) and shows the Agent name, tool and project; only after you confirm that Agent does it call register_agent({}) with an empty object. This never creates an Agent or issues a token.' },
  review_mcp_config: { actor: 'user', text: 'Review the DevLink MCP entry of this Host with `devlink setup status`. It should pass only the DEVLINK_BROKER_FILE path.' },
  open_or_restart_host: { actor: 'user', text: 'After the steps above, close and reopen this Host so the DevLink MCP server starts again.' },
  inspect_owned_agents_then_use: { actor: 'user', text: 'An earlier Agent creation result is unknown. Check your Agents with `devlink agent list` and select the right one with `devlink setup --agent-id <ID>`; do not create another Agent.' },
  retry_diagnosis: { actor: 'user', text: 'The diagnosis did not finish. Run this prompt again; if it repeats, check `devlink status`.' },
});

export interface SetupPlan { ready: boolean; steps: NextAction[] }

/**
 * Pick only the steps the current diagnosis needs. This prompt is delivered through the
 * Host's loaded MCP entry, so the provider's generic review_mcp_config advice is dropped;
 * a wrong broker file still surfaces through the credential/identity stages.
 */
export function setupPlan(data: Connection): SetupPlan {
  const ready = data.hostSession === 'bound' && data.identity === 'verified' && data.credentials === 'valid'
    && data.broker === 'running' && data.central === 'Connected' && !data.issue && !data.clientIssue;
  if (ready) return { ready, steps: [] };
  const steps = [...new Set(data.nextActions)].filter((action) => action !== 'review_mcp_config');
  return { ready, steps: steps.length ? steps : ['retry_diagnosis'] };
}

const rules = [
  'Rules for this reply and the rest of the setup:',
  '- Present the diagnosis and the numbered steps to the user in their language, then ask which step they want to take. Do not call any tool in this reply.',
  '- Call get_my_context({}) or register_agent({}) only after the user explicitly asks for that step in this conversation. register_agent takes {} only.',
  '- Never ask for, accept or repeat passwords, tokens or credential file contents.',
  '- Never create an Agent, re-issue or rotate a token, edit Host or MCP settings, run CLI commands, or create, accept, update or complete tasks as part of setup. Steps marked "you run" are for the user to run in their own terminal.',
];

export function renderSetup(data: Connection): string {
  const plan = setupPlan(data);
  // requestId is provider text inside a model-visible message; pass only plain identifiers.
  const safeId = (value: string) => /^[A-Za-z0-9._:-]{1,128}$/.test(value) ? value : 'invalid';
  const issue = (label: string, value: Connection['issue']) => value ? [`- ${label}: ${value.code} (scope ${value.scope}, outcome ${value.outcome}, requestId ${safeId(value.requestId)})`] : [];
  const lines = [
    'DevLink setup guide (devlink_setup). The connection was diagnosed once, read-only; nothing was registered, created or changed.',
    '',
    'Diagnosis:',
    `- broker ${data.broker}, central ${data.central}, identity ${data.identity}, credentials ${data.credentials}, hostSession ${data.hostSession}`,
    ...issue('issue', data.issue),
    ...issue('clientIssue (this MCP process)', data.clientIssue),
    '',
  ];
  if (plan.ready) {
    lines.push('No setup steps are needed: this Host session is already bound to the verified Agent. If the user wants to see the Agent details, call get_my_context({}) only when they ask.');
  } else {
    lines.push('Needed steps, in order:');
    plan.steps.forEach((step, index) => {
      const guide = setupGuidance[step];
      lines.push(`${index + 1}. [${step}] (${guide.actor === 'user' ? 'you run' : 'assistant, only when you ask'}) ${guide.text}`);
    });
  }
  lines.push('', ...rules);
  return lines.join('\n');
}

export function registerSetupPrompt(server: McpServer, broker: Pick<BrokerClient, 'getConnectionStatus'>): void {
  server.registerPrompt(SETUP_PROMPT_NAME, {
    title: 'DevLink setup',
    description: 'Diagnose this DevLink connection once (read-only) and list only the setup steps still needed. Nothing is registered, created, rotated or executed until you choose a step.',
  }, async (ctx) => {
    const status = await broker.getConnectionStatus({ signal: ctx.mcpReq.signal, mcpRequestId: ctx.mcpReq.id });
    const data = connectionSchema.parse(status.data);
    return {
      description: 'DevLink setup steps for the current connection state',
      messages: [{ role: 'user' as const, content: { type: 'text' as const, text: renderSetup(data) } }],
    };
  });
}
