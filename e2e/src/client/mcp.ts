// LLM Host stand-in: the MCP SDK client from the real-central verify scripts, bound to one actor's broker.
// @ts-expect-error -- plain ESM helper shared with packages/mcp-server/scripts (no type declarations).
import { assertRequiredTools, callTool, connectMcpStdio } from "../../../packages/mcp-server/scripts/lib/mcp-stdio.mjs";
import type { Actor } from "./actor.ts";
import type { Secrets } from "./secrets.ts";

export class McpSession {
  private closed = false;

  private constructor(readonly actor: Actor, private readonly session: { client: any; stderr: string; stderrOverflow: boolean; close(): Promise<void> }, private readonly secrets: Secrets) {}

  static async connect(actor: Actor, entry: string, secrets: Secrets): Promise<McpSession> {
    await actor.registerStoredSecrets();
    const session = await connectMcpStdio({ brokerFile: actor.brokerFile, name: `e2e-${actor.label}`, entry, env: actor.env });
    const mcp = new McpSession(actor, session, secrets);
    try {
      await assertRequiredTools(session.client);
    } catch (error) {
      await mcp.close().catch(() => undefined);
      throw error;
    }
    return mcp;
  }

  call<T = any>(name: string, args: Record<string, unknown> = {}): Promise<T> {
    return callTool(this.session.client, name, args);
  }

  fail<T = any>(name: string, args: Record<string, unknown>, code: string): Promise<T> {
    return callTool(this.session.client, name, args, code);
  }

  /** Calls a tool with arguments its input schema must reject before the broker; returns whether it did. */
  async rejectsInput(name: string, args: Record<string, unknown>): Promise<boolean> {
    try {
      const result = await this.session.client.callTool({ name, arguments: args });
      return result.isError === true && result.structuredContent === undefined;
    } catch (error: any) {
      return error?.code === -32602;
    }
  }

  async close() {
    if (this.closed) return;
    this.closed = true;
    let closeError: unknown;
    await this.session.close().catch((error: unknown) => { closeError = error; });
    this.secrets.assertAbsent(this.session.stderr, `MCP stderr (${this.actor.label})`);
    if (this.session.stderrOverflow) throw new Error(`MCP stderr exceeded the capture limit (${this.actor.label})`);
    await this.actor.log(`[mcp stderr]\n${this.session.stderr}`);
    if (closeError) throw new Error(`MCP session close failed (${this.actor.label}); its stdio child may remain`, { cause: closeError });
  }
}
