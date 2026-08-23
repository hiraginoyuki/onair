import { readFile } from "node:fs/promises";
import { createServer } from "node:http";

const host = process.env.INSPECTOR_BROWSER_TEST_HOST ?? "127.0.0.1";
const port = Number(process.env.INSPECTOR_BROWSER_TEST_PORT ?? "4179");
const artifact = await readFile(new URL("../../dist/index.html", import.meta.url));

const server = createServer((request, response) => {
  const url = new URL(request.url ?? "/", `http://${host}:${port}`);
  if (
    (request.method !== "GET" && request.method !== "HEAD") ||
    url.pathname !== "/_onair/inspector-next"
  ) {
    response.writeHead(404, { "content-type": "text/plain; charset=utf-8" });
    response.end("not found\n");
    return;
  }

  response.writeHead(200, {
    "cache-control": "no-store",
    "content-length": artifact.byteLength,
    "content-type": "text/html; charset=utf-8"
  });
  response.end(request.method === "HEAD" ? undefined : artifact);
});

server.listen(port, host);

function shutdown() {
  server.close((error) => {
    if (error) {
      console.error(error);
      process.exitCode = 1;
    }
  });
}

process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);
