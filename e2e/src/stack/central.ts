import type { ChildProcess } from "node:child_process";
import { randomBytes } from "node:crypto";
import { join } from "node:path";
import type { Postgres } from "./postgres.ts";
import { freePort, isRunning, spawnFailure, spawnLogged, stopChild, waitFor } from "./util.ts";

export interface Central {
  port: number;
  url: string;
  pid: number | undefined;
  /** Generated per run, kept in memory, used only to prove it never reaches logs. */
  cursorSecret: string;
  stop(): Promise<void>;
}

/**
 * Runs central with the default Spring profile: the test profile disables the outbox publisher,
 * and prod sets Secure/SameSite=None cookies that do not fit http loopback.
 */
export async function startCentral(options: { java: string; jar: string; directory: string; postgres: Postgres; frontendOrigin: string }): Promise<Central> {
  const port = await freePort();
  const url = `http://127.0.0.1:${port}`;
  const cursorSecret = randomBytes(48).toString("base64url");
  const env: NodeJS.ProcessEnv = {
    // Drop anything that could switch the profile, port, timezone or JVM flags behind the receipt's back.
    ...Object.fromEntries(Object.entries(process.env).filter(([key]) =>
      !/^(DEVLINK_|SPRING_|DB_|TASK_MESSAGE_|SERVER_|LOGGING_|MANAGEMENT_|CORS_|WS_)/i.test(key)
      && !/^(JAVA_TOOL_OPTIONS|JDK_JAVA_OPTIONS|_JAVA_OPTIONS)$/i.test(key))),
    DB_URL: `jdbc:postgresql://127.0.0.1:${options.postgres.port}/${options.postgres.database}`,
    DB_USERNAME: options.postgres.username,
    DB_PASSWORD: options.postgres.password,
    TASK_MESSAGE_CURSOR_SECRET: cursorSecret,
    CORS_ALLOWED_ORIGINS: options.frontendOrigin,
    WS_ALLOWED_ORIGINS: options.frontendOrigin,
  };
  const child: ChildProcess = spawnLogged(options.java, [
    "-Duser.timezone=UTC", "-Djava.net.preferIPv4Stack=true", "-jar", options.jar,
    "--server.address=127.0.0.1", `--server.port=${port}`,
    // application.yml has no default for this key outside the local/test/prod profiles.
    `--devlink.websocket.allowed-origins=${options.frontendOrigin}`,
  ], { env, cwd: options.directory, logFile: join(options.directory, "central.log") });
  try {
    await waitFor("central health", 60_000, async () => {
      const response = await fetch(`${url}/api/v1/health`, { signal: AbortSignal.timeout(2_000) });
      if (!response.ok) return undefined;
      const body = await response.json() as { success?: boolean };
      return body.success === true ? true : undefined;
    }, () => isRunning(child));
  } catch (error) {
    await stopChild(child).catch(() => undefined);
    const failure = spawnFailure(child);
    throw failure ? new Error(`Cannot start central with ${options.java}: ${failure.message}`) : error;
  }
  return { port, url, pid: child.pid, cursorSecret, stop: () => stopChild(child) };
}
