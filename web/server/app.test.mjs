import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { once } from "node:events";
import { createPortalServer } from "./app.mjs";

async function fixture(t) {
  const root = await mkdtemp(path.join(tmpdir(), "farm-portal-server-"));
  await mkdir(path.join(root, "assets"));
  await writeFile(
    path.join(root, "index.html"),
    '<!doctype html><div id="root">portal</div>',
  );
  await writeFile(path.join(root, "assets/app.js"), 'console.log("portal")');
  // A trailing directory separator matches the production fileURLToPath result.
  const server = createPortalServer({ root: root + path.sep });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  t.after(async () => {
    server.closeAllConnections();
    await new Promise((r) => server.close(r));
    await rm(root, { recursive: true, force: true });
  });
  return `http://127.0.0.1:${server.address().port}`;
}
test("serves the root and SPA deep links with a trailing-slash asset root", async (t) => {
  const base = await fixture(t);
  for (const route of ["/", "/jobs", "/host", "/farms/demo"]) {
    const response = await fetch(base + route);
    assert.equal(response.status, 200);
    assert.match(response.headers.get("content-type"), /text\/html/);
    assert.match(await response.text(), /id="root"/);
  }
  const head = await fetch(base + "/farms/demo", { method: "HEAD" });
  assert.equal(head.status, 200);
  assert.equal(await head.text(), "");
});
test("caches built assets, rejects traversal and does not return HTML for missing assets", async (t) => {
  const base = await fixture(t);
  const asset = await fetch(base + "/assets/app.js");
  assert.equal(asset.status, 200);
  assert.match(asset.headers.get("content-type"), /javascript/);
  assert.equal(
    asset.headers.get("cache-control"),
    "public, max-age=31536000, immutable",
  );
  assert.equal((await fetch(base + "/assets/missing.js")).status, 404);
  assert.equal((await fetch(base + "/..%2f..%2fsecret")).status, 403);
  assert.equal((await fetch(base + "/jobs", { method: "POST" })).status, 405);
});
