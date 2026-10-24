import http from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { readFile } from "node:fs/promises";
import { createReader, ReaderError } from "./reader.js";

const dist = path.resolve(fileURLToPath(new URL("../dist/", import.meta.url)));
const types = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".svg": "image/svg+xml",
};

export function createServer(reader) {
  return http.createServer(async (request, response) => {
    response.setHeader("X-Content-Type-Options", "nosniff");
    response.setHeader("Referrer-Policy", "no-referrer");
    response.setHeader(
      "Content-Security-Policy",
      "default-src 'self'; style-src 'self'; script-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; frame-ancestors 'none'",
    );
    const sendJson = (status, body) => {
      response.writeHead(status, {
        "Content-Type": "application/json",
        "Cache-Control": "no-store",
      });
      response.end(JSON.stringify(body));
    };
    try {
      if (request.method !== "GET" && request.method !== "HEAD") {
        sendJson(405, { error: "This monitor is read only" });
        return;
      }
      const url = new URL(request.url, "http://localhost");
      if (url.pathname === "/api/runs") {
        sendJson(200, await reader.list());
        return;
      }
      if (url.pathname.startsWith("/api/runs/")) {
        const id = decodeURIComponent(url.pathname.slice("/api/runs/".length));
        sendJson(200, await reader.snapshot(id));
        return;
      }
      if (url.pathname.startsWith("/api/")) {
        sendJson(404, { error: "Endpoint not found" });
        return;
      }
      const asset =
        url.pathname === "/"
          ? "index.html"
          : decodeURIComponent(url.pathname.slice(1));
      const location = path.resolve(dist, asset);
      if (!location.startsWith(`${dist}${path.sep}`)) {
        sendJson(404, { error: "Not found" });
        return;
      }
      try {
        const content = await readFile(location);
        response.writeHead(200, {
          "Content-Type":
            types[path.extname(location)] ?? "application/octet-stream",
          "Cache-Control": "no-cache",
        });
        response.end(request.method === "HEAD" ? undefined : content);
      } catch (error) {
        if (error.code === "ENOENT") sendJson(404, { error: "Not found" });
        else throw error;
      }
    } catch (error) {
      sendJson(error instanceof ReaderError ? error.status : 500, {
        error:
          error instanceof ReaderError
            ? error.message
            : "Cannot read artifact storage",
      });
    }
  });
}

if (
  process.argv[1] &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)
) {
  try {
    if (!process.env.ARTIFACTS_DIR)
      throw new Error(
        "ARTIFACTS_DIR must point to the directory containing run UUID directories",
      );
    const reader = await createReader(process.env.ARTIFACTS_DIR);
    const host = process.env.HOST ?? "127.0.0.1";
    const port = Number(process.env.PORT ?? 4173);
    if (!Number.isInteger(port) || port < 1 || port > 65535)
      throw new Error("PORT must be between 1 and 65535");
    const server = createServer(reader);
    server.listen(port, host, () =>
      process.stdout.write(
        `Crabber monitor listening on http://${host}:${port}\n`,
      ),
    );
    for (const signal of ["SIGINT", "SIGTERM"])
      process.on(signal, () => server.close(() => process.exit(0)));
  } catch (error) {
    process.stderr.write(`${error.message}\n`);
    process.exitCode = 1;
  }
}
