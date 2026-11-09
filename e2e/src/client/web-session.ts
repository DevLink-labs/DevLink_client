import type { Secrets } from "./secrets.ts";

/** Minimal central web client (session cookie + CSRF) used by fixtures that seed data without the UI. */
export class WebSession {
  private cookie = "";

  constructor(private readonly centralUrl: string, private readonly secrets: Secrets) {}

  private remember(response: Response) {
    const session = response.headers.getSetCookie().find((value) => value.startsWith("JSESSIONID="));
    if (session) {
      this.cookie = session.split(";")[0];
      this.secrets.add(this.cookie.slice("JSESSIONID=".length));
    }
  }

  private async csrf(): Promise<{ headerName: string; token: string }> {
    const response = await fetch(new URL("/api/v1/auth/csrf", this.centralUrl), { headers: this.cookie ? { Cookie: this.cookie } : {} });
    if (!response.ok) throw new Error(`csrf -> ${response.status}`);
    this.remember(response);
    return ((await response.json()) as { data: { headerName: string; token: string } }).data;
  }

  async send<T = any>(method: string, path: string, body?: unknown, expected = [200, 201]): Promise<T> {
    const csrf = await this.csrf();
    const response = await fetch(new URL(path, this.centralUrl), {
      method,
      headers: { Cookie: this.cookie, [csrf.headerName]: csrf.token, ...(body === undefined ? {} : { "Content-Type": "application/json" }) },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    this.remember(response);
    if (!expected.includes(response.status)) throw new Error(`${method} ${path} -> ${response.status}`);
    const value = await response.json().catch(() => ({})) as { data?: T };
    return value.data as T;
  }

  signup(user: { email: string; password: string; name: string }) {
    return this.send("POST", "/api/v1/auth/signup", user, [201]);
  }

  login(user: { email: string; password: string }) {
    return this.send<{ userId: number }>("POST", "/api/v1/auth/login", { email: user.email, password: user.password });
  }
}
