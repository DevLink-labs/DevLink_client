import { createHash } from "node:crypto";
import { readdir, readFile, stat } from "node:fs/promises";
import { join } from "node:path";

const digest = (value: string) => createHash("sha256").update(value).digest("hex");

/** Generated secrets are single base64url/hex/JWT-like tokens; split text on anything else. */
const tokens = (text: string) => text.split(/[^A-Za-z0-9_\-.~+/=]+/).filter((token) => token.length >= 8);

/** In-memory registry of every password/token a test produced or read. Never serialized as plain text. */
export class Secrets {
  private readonly values = new Set<string>();

  add(value: string | undefined | null) {
    if (value && value.length >= 8) this.values.add(value);
  }

  /** SHA-256 of each secret: safe to write down, lets the runner find them in files written later. */
  digests(): string[] {
    return [...this.values].map(digest);
  }

  /**
   * Runner-side check for files Playwright writes after test fixtures finish (error-context.md and
   * other attachments under test-results): hashes every token and compares with recorded digests.
   */
  static async assertDigestsAbsent(paths: string[], digests: Set<string>) {
    if (digests.size === 0) return;
    const visit = async (path: string): Promise<void> => {
      const info = await stat(path).catch(() => undefined);
      if (!info) return;
      if (info.isDirectory()) {
        for (const entry of await readdir(path)) await visit(join(path, entry));
      } else if (/\.(log|json|txt|md|jsonl|toml)$/i.test(path)) {
        if (info.size > 20_000_000) throw new Error(`Cannot scan ${path} for secrets: larger than 20 MB`);
        // Also try the token without leading/trailing '.', '=', '+', '/', '~' (sentence punctuation, padding).
        for (const token of tokens(await readFile(path, "utf8"))) {
          // Registered secrets contain no '=', '/', '+' and at most inner '.', so also try the pieces
          // between those characters (e.g. `password=SECRET`, `/SECRET.`).
          for (const candidate of new Set([token, token.replace(/^[.=+/~]+|[.=+/~]+$/g, ""), ...token.split(/[=+/]+/), ...token.split(/[=+/.]+/)])) {
            if (digests.has(digest(candidate))) throw new Error(`A generated secret appeared in ${path}`);
          }
        }
      }
    };
    for (const path of paths) await visit(path);
  }

  /** Throws when any registered secret occurs in `text`; the message names only the source. */
  assertAbsent(text: string, source: string) {
    for (const value of this.values) {
      if (text.includes(value)) throw new Error(`A generated secret appeared in ${source}`);
    }
  }

  /** Scans files (recursively for directories) that are kept after the run: logs, receipt, results. */
  async assertAbsentInFiles(paths: string[]) {
    for (const path of paths) {
      const info = await stat(path).catch(() => undefined);
      if (!info) continue;
      if (info.isDirectory()) {
        const entries = await readdir(path);
        await this.assertAbsentInFiles(entries.map((entry) => join(path, entry)));
      } else if (/\.(log|json|txt|md|jsonl|toml)$/i.test(path)) {
        if (info.size > 20_000_000) throw new Error(`Cannot scan ${path} for secrets: larger than 20 MB`);
        this.assertAbsent(await readFile(path, "utf8"), path);
      }
    }
  }
}
