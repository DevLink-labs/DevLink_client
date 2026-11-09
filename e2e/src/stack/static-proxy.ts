import { createReadStream } from "node:fs";
import { stat } from "node:fs/promises";
import { createServer, request, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import { extname, join, normalize, sep } from "node:path";

const types: Record<string, string> = {
  ".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8", ".css": "text/css; charset=utf-8",
  ".json": "application/json", ".svg": "image/svg+xml", ".png": "image/png", ".jpg": "image/jpeg", ".webp": "image/webp",
  ".glb": "model/gltf-binary", ".gltf": "model/gltf+json", ".ico": "image/x-icon", ".woff2": "font/woff2", ".txt": "text/plain",
};

export interface StaticProxy {
  server: Server;
  port: number;
  origin: string;
  setTarget(url: string): void;
  close(): Promise<void>;
}

/**
 * Serves the built frontend and forwards /api to central. Like vite.config.ts it drops the Origin
 * header so central sees a same-origin request; session cookies and CSRF stay central's concern.
 */
export async function startStaticProxy(dist: string): Promise<StaticProxy> {
  let target: URL | undefined;
  const root = normalize(dist + sep);

  async function serveFile(req: IncomingMessage, res: ServerResponse) {
    const path = decodeURIComponent(new URL(req.url ?? "/", "http://local").pathname);
    let file = normalize(join(root, path));
    if (!file.startsWith(root)) { res.writeHead(403).end(); return; }
    const info = await stat(file).catch(() => undefined);
    if (!info?.isFile()) file = join(root, "index.html"); // SPA fallback
    res.writeHead(200, { "Content-Type": types[extname(file)] ?? "application/octet-stream", "Cache-Control": "no-store" });
    createReadStream(file).pipe(res);
  }

  function forward(req: IncomingMessage, res: ServerResponse) {
    if (!target) { res.writeHead(503).end(); return; }
    const headers = { ...req.headers, host: target.host };
    delete headers.origin;
    const upstream = request({ hostname: target.hostname, port: target.port, method: req.method, path: req.url, headers }, (response) => {
      res.writeHead(response.statusCode ?? 502, response.headers);
      response.pipe(res);
    });
    upstream.on("error", () => { if (!res.headersSent) res.writeHead(502); res.end(); });
    req.pipe(upstream);
  }

  const server = createServer((req, res) => {
    const pathname = (req.url ?? "/").split("?")[0];
    if (pathname === "/api" || pathname.startsWith("/api/")) forward(req, res);
    else serveFile(req, res).catch(() => { if (!res.headersSent) res.writeHead(500); res.end(); });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (!address || typeof address !== "object") throw new Error("Proxy has no address");
  return {
    server, port: address.port, origin: `http://127.0.0.1:${address.port}`,
    setTarget(url) { target = new URL(url); },
    close: () => new Promise<void>((resolve) => { server.closeAllConnections(); server.close(() => resolve()); }),
  };
}
