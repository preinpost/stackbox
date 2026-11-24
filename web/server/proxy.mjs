import http from "node:http";
import https from "node:https";

const json = (res, status, error) => {
  if (res.headersSent) return res.destroy();
  res.writeHead(status, {
    "content-type": "application/json",
    "cache-control": "no-store",
  });
  res.end(JSON.stringify({ error }));
};

/** stackboxd URL for the request's path and query, or null when STACKBOX_API_URL is unusable. */
function upstreamUrl(req, env) {
  try {
    const url = new URL(env.STACKBOX_API_URL || "http://127.0.0.1:10010");
    if (
      !["http:", "https:"].includes(url.protocol) ||
      url.username ||
      url.password
    )
      return null;
    const requested = new URL(req.url, "http://portal.local");
    url.pathname = requested.pathname;
    url.search = requested.search;
    return url;
  } catch {
    return null;
  }
}

/**
 * The portal's API surface, including canonical stackbox IDs: every route the browser may reach
 * through this boundary. Encoded or alternate spellings cannot bypass the credential
 * response filter. A route the portal calls but this does not list fails as a 404, so
 * `proxy.test.mjs` checks every path of `docs/openapi.json` against this expression.
 */
export const portalApiPath =
  /^\/api\/v1\/(?:host|local-host|agent-hosts|hosts(?:\/[a-z0-9-]+(?:\/(?:check|prepare|agent))?)?|host-jobs(?:\/[a-f0-9-]+)?|catalog|plans|releases(?:\/(?:plan|stackboxes))?|openapi\.json|jobs(?:\/[a-f0-9-]+)?|stackboxes(?:\/[a-z0-9-]+(?:\/(?:deploy|computes|settle|endpoints|instances(?:\/[a-z0-9-]+\/gpu-load)?|nodes\/[a-z0-9-]+\/faults|gpu\/(?:guest-script|user-data)|snapshots|logs(?:\/stream)?))?)?|snapshots(?:\/[a-z0-9-]+(?:\/(?:export|copies\/[a-z0-9-]+))?)?|buckets(?:\/[a-z0-9-]+(?:\/(?:check|objects))?)?|relay(?:\/cloudflared\/update)?)$/;

/** Hostname of a Host header or config value (no port, no IPv6 brackets), or null. */
function hostName(value) {
  const text = String(value ?? "")
    .trim()
    .toLowerCase();
  const bracketed = text.match(/^\[([0-9a-f:.]+)\](?::\d+)?$/);
  if (bracketed) return bracketed[1];
  // A bare IPv6 address from configuration, e.g. fd00::5.
  if (text.split(":").length > 2) return text;
  return text.match(/^([a-z0-9.-]+)(?::\d+)?$/)?.[1] ?? null;
}

/**
 * Host names the portal answers to: loopback, PORTAL_HOST and PORTAL_ALLOWED_HOSTS
 * (comma-separated). Wildcard bind addresses are not names a browser can use.
 */
function allowedHostNames(env) {
  const names = new Set(["localhost", "127.0.0.1", "::1"]);
  const configured = [
    env.PORTAL_HOST,
    ...String(env.PORTAL_ALLOWED_HOSTS || "").split(","),
  ];
  for (const value of configured) {
    const name = hostName(value);
    if (name && !["0.0.0.0", "::"].includes(name)) names.add(name);
  }
  return names;
}

/** DNS rebinding guard: the Host name must be one the portal serves, whatever the Origin. */
function hostAllowed(req, env) {
  const name = hostName(req.headers.host);
  return name !== null && allowedHostNames(env).has(name);
}

/** Shared dev/production boundary. Secrets remain on this side of the connection. */
export function proxyApi(req, res, env = process.env) {
  if (!req.url?.startsWith("/api/v1/"))
    return json(res, 404, "API 경로를 찾을 수 없습니다.");
  if (!hostAllowed(req, env))
    return json(res, 403, "허용되지 않은 Host 헤더입니다.");
  if (req.headers.origin) {
    try {
      if (new URL(req.headers.origin).host !== req.headers.host)
        return json(res, 403, "다른 출처의 API 요청은 허용하지 않습니다.");
    } catch {
      return json(res, 403, "잘못된 Origin 헤더입니다.");
    }
  }
  const url = upstreamUrl(req, env);
  if (!url) return json(res, 502, "서버의 STACKBOX_API_URL 설정을 확인하세요.");

  // Only expose the portal's known API surface, including canonical stackbox IDs.
  // Encoded/alternate spellings cannot bypass the credential response filter.
  if (!portalApiPath.test(url.pathname)) {
    return json(res, 404, "API 경로를 찾을 수 없습니다.");
  }
  const headers = { accept: req.headers.accept || "application/json" };
  for (const name of ["content-type", "content-length", "last-event-id"]) {
    if (req.headers[name]) headers[name] = req.headers[name];
  }
  if (env.STACKBOX_API_TOKEN)
    headers.authorization = `Bearer ${env.STACKBOX_API_TOKEN}`;
  const upstream = (url.protocol === "https:" ? https : http).request(
    url,
    {
      method: req.method,
      headers,
      // Streaming logs deliberately have no request timeout.
      timeout: url.pathname.endsWith("/logs/stream") ? 0 : 30_000,
    },
    (response) => {
      const responseHeaders = {
        "content-type": response.headers["content-type"] || "application/json",
        "cache-control": "no-store",
      };
      if (response.headers.location)
        responseHeaders.location = response.headers.location;
      // Horizon shows the admin login. Forward only those fields, not any extra payload.
      if (/\/stackboxes\/[^/]+\/endpoints$/.test(url.pathname)) {
        let size = 0;
        const chunks = [];
        response.on("data", (chunk) => {
          size += chunk.length;
          if (size > 1_048_576) {
            response.destroy();
            json(res, 502, "Endpoint 응답이 너무 큽니다.");
          } else chunks.push(chunk);
        });
        response.on("end", () => {
          if (res.writableEnded) return;
          let value;
          try {
            const raw = JSON.parse(Buffer.concat(chunks).toString());
            const loginKeys = [
              "OS_USERNAME",
              "OS_PASSWORD",
              "OS_USER_DOMAIN_NAME",
              "OS_PROJECT_NAME",
              "OS_PROJECT_DOMAIN_NAME",
            ];
            const credentials = Object.fromEntries(
              loginKeys
                .filter((key) => raw.credentials?.[key])
                .map((key) => [key, raw.credentials[key]]),
            );
            value =
              response.statusCode === 200
                ? {
                    services: raw.services,
                    credentials,
                    // Published data stores: host port, account and databases.
                    datastores: Array.isArray(raw.datastores)
                      ? raw.datastores.map((store) => ({
                          id: store.id,
                          label: store.label,
                          protocol: store.protocol,
                          host: store.host,
                          port: store.port,
                          username: store.username,
                          password: store.password,
                          vhost: store.vhost,
                          url: store.url,
                          databases: Array.isArray(store.databases)
                            ? store.databases
                            : [],
                        }))
                      : [],
                  }
                : { error: raw.error || "Endpoint 조회에 실패했습니다." };
          } catch {
            return json(res, 502, "Endpoint 응답을 읽을 수 없습니다.");
          }
          res.writeHead(response.statusCode || 502, responseHeaders);
          res.end(JSON.stringify(value));
        });
        response.on("error", () =>
          json(res, 502, "Endpoint 연결이 종료되었습니다."),
        );
      } else {
        if (url.pathname.endsWith("/logs/stream"))
          responseHeaders["x-accel-buffering"] = "no";
        res.writeHead(response.statusCode || 502, responseHeaders);
        res.flushHeaders();
        response.pipe(res);
        response.on("error", () => res.destroy());
      }
    },
  );
  upstream.on("timeout", () => upstream.destroy(new Error("timeout")));
  upstream.on("error", () =>
    json(
      res,
      502,
      "stackboxd에 연결할 수 없습니다. 데몬 주소와 서버 token 설정을 확인하세요.",
    ),
  );
  req.on("aborted", () => upstream.destroy());
  res.on("close", () => upstream.destroy());
  req.pipe(upstream);
}

const refuse = (socket, status, error) => {
  const body = JSON.stringify({ error });
  socket.end(
    `HTTP/1.1 ${status} ${http.STATUS_CODES[status]}\r\n` +
      "content-type: application/json\r\n" +
      `content-length: ${Buffer.byteLength(body)}\r\n` +
      "connection: close\r\n\r\n" +
      body,
  );
};

/**
 * WebSocket counterpart of proxyApi, for the stackbox shell only. A browser opens a WebSocket
 * without CORS, so the page's Origin must be this portal, and it must name one.
 */
export function proxyUpgrade(req, socket, head, env = process.env) {
  socket.on("error", () => socket.destroy());
  if (!hostAllowed(req, env))
    return refuse(socket, 403, "허용되지 않은 Host 헤더입니다.");
  try {
    if (new URL(req.headers.origin).host !== req.headers.host)
      return refuse(socket, 403, "다른 출처의 셸 연결은 허용하지 않습니다.");
  } catch {
    return refuse(socket, 403, "Origin 헤더가 없거나 잘못되었습니다.");
  }
  if (req.headers.upgrade?.toLowerCase() !== "websocket")
    return refuse(socket, 400, "WebSocket 연결만 지원합니다.");
  const url = upstreamUrl(req, env);
  if (!url)
    return refuse(socket, 502, "서버의 STACKBOX_API_URL 설정을 확인하세요.");
  const rows = url.searchParams.get("rows");
  const cols = url.searchParams.get("cols");
  const node = url.searchParams.get("node");
  if (
    !/^\/api\/v1\/stackboxes\/[a-z0-9-]+\/shell$/.test(url.pathname) ||
    !/^\d{1,4}$/.test(rows ?? "") ||
    !/^\d{1,4}$/.test(cols ?? "") ||
    (node !== null && !/^(control|compute-\d{1,2})$/.test(node))
  )
    return refuse(socket, 404, "API 경로를 찾을 수 없습니다.");
  url.search = `?rows=${rows}&cols=${cols}${node === null ? "" : `&node=${node}`}`;
  const headers = { connection: "Upgrade", upgrade: "websocket" };
  for (const name of [
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-extensions",
  ]) {
    if (req.headers[name]) headers[name] = req.headers[name];
  }
  if (env.STACKBOX_API_TOKEN)
    headers.authorization = `Bearer ${env.STACKBOX_API_TOKEN}`;
  const upstream = (url.protocol === "https:" ? https : http).request(url, {
    headers,
    timeout: 30_000,
  });
  upstream.on("upgrade", (response, tunnel, tunnelHead) => {
    const lines = ["HTTP/1.1 101 Switching Protocols"];
    for (const name of [
      "upgrade",
      "connection",
      "sec-websocket-accept",
      "sec-websocket-extensions",
    ]) {
      if (response.headers[name])
        lines.push(`${name}: ${response.headers[name]}`);
    }
    socket.write(lines.join("\r\n") + "\r\n\r\n");
    if (tunnelHead?.length) socket.write(tunnelHead);
    if (head?.length) tunnel.write(head);
    const close = () => {
      socket.destroy();
      tunnel.destroy();
    };
    tunnel.on("error", close).on("close", close);
    socket.on("close", close);
    tunnel.pipe(socket);
    socket.pipe(tunnel);
  });
  // stackboxd refused the upgrade: relay its status and error message.
  upstream.on("response", (response) => {
    let body = "";
    response.setEncoding("utf8");
    response.on("data", (chunk) => {
      if (body.length < 4096) body += chunk;
    });
    response.on("end", () => {
      let error = "셸 연결이 거절되었습니다.";
      try {
        error = JSON.parse(body).error || error;
      } catch {}
      refuse(socket, response.statusCode || 502, error);
    });
  });
  upstream.on("timeout", () => upstream.destroy(new Error("timeout")));
  upstream.on("error", () => {
    if (!socket.destroyed)
      refuse(
        socket,
        502,
        "stackboxd에 연결할 수 없습니다. 데몬 주소와 서버 token 설정을 확인하세요.",
      );
  });
  socket.on("close", () => upstream.destroy());
  upstream.end();
}
