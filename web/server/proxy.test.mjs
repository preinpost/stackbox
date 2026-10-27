import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { once } from "node:events";
import { proxyApi, proxyUpgrade } from "./proxy.mjs";

async function listen(server) {
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  return `http://127.0.0.1:${server.address().port}`;
}
async function setup(t, handler) {
  const upstream = http.createServer(handler);
  const url = await listen(upstream);
  const portal = http.createServer((req, res) =>
    proxyApi(req, res, {
      FARM_API_URL: url,
      FARM_API_TOKEN: "server-only-token",
    }),
  );
  const address = await listen(portal);
  t.after(() => {
    portal.closeAllConnections();
    portal.close();
    upstream.closeAllConnections();
    upstream.close();
  });
  return address;
}
test("injects server token and forwards method, JSON body, status and Location", async (t) => {
  const address = await setup(t, async (req, res) => {
    assert.equal(req.headers.authorization, "Bearer server-only-token");
    assert.equal(req.method, "POST");
    let body = "";
    for await (const chunk of req) body += chunk;
    assert.deepEqual(JSON.parse(body), { id: "demo" });
    res.writeHead(202, {
      "content-type": "application/json",
      location: "/api/v1/jobs/abc",
    });
    res.end('{"id":"abc"}');
  });
  const response = await fetch(`${address}/api/v1/farms`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      authorization: "Bearer browser-token",
    },
    body: '{"id":"demo"}',
  });
  assert.equal(response.status, 202);
  assert.equal(response.headers.get("location"), "/api/v1/jobs/abc");
  assert.equal(response.headers.get("cache-control"), "no-store");
  assert.deepEqual(await response.json(), { id: "abc" });
});
test("forwards the Horizon login and drops unrelated endpoint secrets", async (t) => {
  const address = await setup(t, (_req, res) => {
    res.setHeader("content-type", "application/json");
    res.end(
      JSON.stringify({
        services: { keystone: "http://localhost:15000/v3" },
        credentials: {
          OS_USERNAME: "admin",
          OS_PASSWORD: "horizon-login",
          OS_USER_DOMAIN_NAME: "Default",
          OS_TOKEN: "not-a-login-field",
        },
        datastores: [
          {
            id: "mariadb",
            label: "MariaDB",
            protocol: "mysql",
            host: "localhost",
            port: 15006,
            username: "root",
            password: "database-secret",
            url: "mysql://root@localhost:15006/",
            databases: ["keystone", "nova"],
            internal_address: "10.254.0.2",
          },
        ],
        extra_secret: "hidden",
      }),
    );
  });
  const response = await fetch(`${address}/api/v1/farms/demo/endpoints`);
  assert.deepEqual(await response.json(), {
    services: { keystone: "http://localhost:15000/v3" },
    credentials: {
      OS_USERNAME: "admin",
      OS_PASSWORD: "horizon-login",
      OS_USER_DOMAIN_NAME: "Default",
    },
    // Data stores pass through field by field; the guest's own address stays behind.
    datastores: [
      {
        id: "mariadb",
        label: "MariaDB",
        protocol: "mysql",
        host: "localhost",
        port: 15006,
        username: "root",
        password: "database-secret",
        url: "mysql://root@localhost:15006/",
        databases: ["keystone", "nova"],
      },
    ],
  });
  assert.deepEqual(
    JSON.parse(
      JSON.stringify(
        await (await fetch(`${address}/api/v1/farms/demo/endpoints`)).json(),
      ),
    ).datastores[0].internal_address,
    undefined,
  );
});
test("forwards host registration credentials only in the upstream request and permits host jobs", async (t) => {
  const address = await setup(t, async (req, res) => {
    assert.equal(req.headers.authorization, "Bearer server-only-token");
    if (req.url === "/api/v1/hosts" && req.method === "POST") {
      let body = "";
      for await (const chunk of req) body += chunk;
      assert.equal(
        JSON.parse(body).credentials.password,
        "test-only-ssh-password",
      );
      res.writeHead(201, { "content-type": "application/json" });
      res.end('{"id":"target","auth_method":"password"}');
    } else {
      res.setHeader("content-type", "application/json");
      res.end("[]");
    }
  });
  const response = await fetch(`${address}/api/v1/hosts`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      id: "target",
      credentials: { method: "password", password: "test-only-ssh-password" },
    }),
  });
  assert.equal(response.status, 201);
  assert.equal(response.headers.get("cache-control"), "no-store");
  assert.equal(
    (await response.text()).includes("test-only-ssh-password"),
    false,
  );
  for (const path of [
    "/hosts/fingerprint",
    "/hosts/target/check",
    "/hosts/target/prepare",
    "/host-jobs",
    "/host-jobs/abc-123",
  ]) {
    assert.equal((await fetch(`${address}/api/v1${path}`)).status, 200);
  }
  for (const path of [
    "/hosts/target/credentials",
    "/hosts/target/exec",
    "/hosts/%74arget/check",
  ]) {
    assert.equal((await fetch(`${address}/api/v1${path}`)).status, 404);
  }
});
test("rejects cross-origin mutations, unsupported routes and encoded credential-route spellings", async (t) => {
  const address = await setup(t, () => {
    assert.fail("request must not reach farmd");
  });
  for (const path of [
    "/api/v1/farms/demo/%65ndpoints",
    "/api/v1/unknown",
    "/api/v1/farms/demo/credentials",
  ]) {
    assert.equal((await fetch(address + path)).status, 404);
  }
  assert.equal(
    (
      await fetch(`${address}/api/v1/farms`, {
        method: "POST",
        headers: { origin: "https://other.example" },
      })
    ).status,
    403,
  );
});
test("forwards snapshot routes and keeps others closed", async (t) => {
  const seen = [];
  const address = await setup(t, (req, res) => {
    seen.push(`${req.method} ${req.url}`);
    res.writeHead(202, { "content-type": "application/json" });
    res.end("{}");
  });
  for (const [method, path] of [
    ["POST", "/api/v1/farms/demo/snapshots"],
    ["GET", "/api/v1/snapshots"],
    ["POST", "/api/v1/snapshots/import"],
    ["DELETE", "/api/v1/snapshots/demo-clean"],
    ["POST", "/api/v1/snapshots/demo-clean/export"],
  ]) {
    const response = await fetch(address + path, { method });
    assert.equal(response.status, 202, path);
  }
  assert.equal(seen.length, 5);
  for (const path of [
    "/api/v1/snapshots/demo/restore",
    "/api/v1/snapshots/Demo",
    "/api/v1/farms/demo/snapshots/x",
  ]) {
    assert.equal((await fetch(address + path)).status, 404, path);
  }
  assert.equal(seen.length, 5);
});
test("streams SSE and propagates resume cursors without buffering", async (t) => {
  const address = await setup(t, (req, res) => {
    assert.equal(req.headers["last-event-id"], "12");
    assert.equal(req.url, "/api/v1/farms/demo/logs/stream?after=12");
    res.writeHead(200, { "content-type": "text/event-stream" });
    res.write('event: log\nid: 13\ndata: {"id":13,"message":"hello"}\n\n');
    // Keep the upstream stream open: the client must receive the event immediately.
  });
  const response = await fetch(
    `${address}/api/v1/farms/demo/logs/stream?after=12`,
    { headers: { "last-event-id": "12" }, signal: AbortSignal.timeout(2000) },
  );
  assert.equal(response.headers.get("content-type"), "text/event-stream");
  const reader = response.body.getReader();
  const { value } = await reader.read();
  assert.match(new TextDecoder().decode(value), /id: 13/);
  await reader.cancel();
});
test("returns an actionable error when farmd is unreachable", async (t) => {
  const upstream = http.createServer();
  const url = await listen(upstream);
  await new Promise((resolve) => upstream.close(resolve));
  const portal = http.createServer((req, res) =>
    proxyApi(req, res, { FARM_API_URL: url }),
  );
  const address = await listen(portal);
  t.after(() => {
    portal.closeAllConnections();
    portal.close();
  });
  const response = await fetch(`${address}/api/v1/farms`);
  assert.equal(response.status, 502);
  assert.match((await response.json()).error, /farmd/);
});

test("flushes SSE headers before the first event or keepalive", async (t) => {
  const address = await setup(t, (_req, res) => {
    res.writeHead(200, { "content-type": "text/event-stream" });
    res.flushHeaders();
  });
  const response = await fetch(`${address}/api/v1/farms/demo/logs/stream`, {
    signal: AbortSignal.timeout(1500),
  });
  assert.equal(response.status, 200);
  await response.body.cancel();
});

async function upgradeSetup(t, onUpgrade) {
  const upstream = http.createServer((_req, res) => {
    res.writeHead(409, { "content-type": "application/json" });
    res.end('{"error":"the shell needs the farm\'s VM to be running"}');
  });
  if (onUpgrade) upstream.on("upgrade", onUpgrade);
  const url = await listen(upstream);
  const portal = http.createServer();
  portal.on("upgrade", (req, socket, head) =>
    proxyUpgrade(req, socket, head, {
      FARM_API_URL: url,
      FARM_API_TOKEN: "server-only-token",
    }),
  );
  const address = await listen(portal);
  t.after(() => {
    portal.closeAllConnections();
    portal.close();
    upstream.closeAllConnections();
    upstream.close();
  });
  return address;
}
function openSocket(address, path, origin = address) {
  const request = http.request(`${address}${path}`, {
    headers: {
      connection: "Upgrade",
      upgrade: "websocket",
      "sec-websocket-key": "dGhlIHNhbXBsZSBub25jZQ==",
      "sec-websocket-version": "13",
      ...(origin ? { origin } : {}),
    },
  });
  request.end();
  return new Promise((resolve, reject) => {
    request.on("upgrade", (response, socket) => resolve({ response, socket }));
    request.on("response", (response) => resolve({ response }));
    request.on("error", reject);
  });
}
test("tunnels the shell WebSocket with the server token and normalized query", async (t) => {
  const address = await upgradeSetup(t, (req, socket) => {
    assert.equal(req.headers.authorization, "Bearer server-only-token");
    assert.equal(req.url, "/api/v1/farms/demo/shell?rows=24&cols=80");
    assert.equal(req.headers.origin, undefined);
    socket.write(
      "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-accept: fixture\r\n\r\n",
    );
    socket.pipe(socket);
  });
  const { response, socket } = await openSocket(
    address,
    "/api/v1/farms/demo/shell?rows=24&cols=80&token=leak",
  );
  assert.equal(response.statusCode, 101);
  assert.equal(response.headers["sec-websocket-accept"], "fixture");
  socket.write("echo");
  const [echo] = await once(socket, "data");
  assert.equal(echo.toString(), "echo");
  socket.destroy();
});
test("refuses cross-origin, origin-less and unknown shell upgrades and relays farmd refusals", async (t) => {
  const address = await upgradeSetup(t);
  const path = "/api/v1/farms/demo/shell?rows=24&cols=80";
  for (const origin of ["https://evil.example", null]) {
    const { response } = await openSocket(address, path, origin);
    assert.equal(response.statusCode, 403);
  }
  for (const other of [
    "/api/v1/farms/demo/logs?rows=24&cols=80",
    "/api/v1/farms/demo/shell?rows=x&cols=80",
  ]) {
    const { response } = await openSocket(address, other);
    assert.equal(response.statusCode, 404);
  }
  const { response } = await openSocket(address, path);
  assert.equal(response.statusCode, 409);
  let body = "";
  for await (const chunk of response) body += chunk;
  assert.match(JSON.parse(body).error, /running/);
});
