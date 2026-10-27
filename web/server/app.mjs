import http from "node:http";
import { createReadStream } from "node:fs";
import { stat } from "node:fs/promises";
import path from "node:path";
import { proxyApi, proxyUpgrade } from "./proxy.mjs";

export function createPortalServer({ root, env = process.env }) {
  root = path.resolve(root);
  const mime = {
    ".html": "text/html; charset=utf-8",
    ".js": "text/javascript",
    ".css": "text/css",
    ".svg": "image/svg+xml",
    ".woff2": "font/woff2",
    ".png": "image/png",
  };
  const server = http.createServer(async (req, res) => {
    if (req.url?.startsWith("/api/")) return proxyApi(req, res, env);
    if (!["GET", "HEAD"].includes(req.method || "")) {
      res.writeHead(405);
      return res.end();
    }
    try {
      const pathname = decodeURIComponent(
        new URL(req.url || "/", "http://portal.local").pathname,
      );
      const relative = pathname.replace(/^\/+/, "");
      let file = path.resolve(root, relative);
      if (file !== root && !file.startsWith(root + path.sep)) {
        res.writeHead(403);
        return res.end();
      }
      let info = await stat(file).catch(() => null);
      if (!info?.isFile()) {
        if (pathname.startsWith("/assets/") || path.extname(pathname)) {
          res.writeHead(404);
          return res.end();
        }
        file = path.join(root, "index.html");
        info = await stat(file);
      }
      res.writeHead(200, {
        "content-type": mime[path.extname(file)] || "application/octet-stream",
        "cache-control": file.includes(`${path.sep}assets${path.sep}`)
          ? "public, max-age=31536000, immutable"
          : "no-cache",
        "x-content-type-options": "nosniff",
      });
      if (req.method === "HEAD") res.end();
      else
        createReadStream(file)
          .on("error", () => res.destroy())
          .pipe(res);
    } catch {
      res.writeHead(400);
      res.end("Bad request");
    }
  });
  server.on("upgrade", (req, socket, head) => {
    if (req.url?.startsWith("/api/")) proxyUpgrade(req, socket, head, env);
    else socket.destroy();
  });
  return server;
}
