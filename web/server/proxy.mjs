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

/** Shared dev/production boundary. Secrets remain on this side of the connection. */
export function proxyApi(req, res, env = process.env) {
  if (!req.url?.startsWith("/api/v1/"))
    return json(res, 404, "API 경로를 찾을 수 없습니다.");
  if (req.headers.origin) {
    try {
      if (new URL(req.headers.origin).host !== req.headers.host)
        return json(res, 403, "다른 출처의 API 요청은 허용하지 않습니다.");
    } catch {
      return json(res, 403, "잘못된 Origin 헤더입니다.");
    }
  }
  let url;
  try {
    url = new URL(env.FARM_API_URL || "http://127.0.0.1:8080");
    if (
      !["http:", "https:"].includes(url.protocol) ||
      url.username ||
      url.password
    )
      throw new Error();
    const requested = new URL(req.url, "http://portal.local");
    url.pathname = requested.pathname;
    url.search = requested.search;
  } catch {
    return json(res, 502, "서버의 FARM_API_URL 설정을 확인하세요.");
  }

  // Only expose the portal's known API surface, including canonical farm IDs.
  // Encoded/alternate spellings cannot bypass the credential response filter.
  if (
    !/^\/api\/v1\/(?:host|hosts(?:\/[a-z0-9-]+(?:\/(?:check|prepare|agent))?)?|host-jobs(?:\/[a-f0-9-]+)?|catalog|plans|openapi\.json|jobs(?:\/[a-f0-9-]+)?|farms(?:\/[a-z0-9-]+(?:\/(?:deploy|endpoints|snapshots|logs(?:\/stream)?))?)?|snapshots(?:\/[a-z0-9-]+(?:\/export)?)?)$/.test(
      url.pathname,
    )
  ) {
    return json(res, 404, "API 경로를 찾을 수 없습니다.");
  }
  const headers = { accept: req.headers.accept || "application/json" };
  for (const name of ["content-type", "content-length", "last-event-id"]) {
    if (req.headers[name]) headers[name] = req.headers[name];
  }
  if (env.FARM_API_TOKEN)
    headers.authorization = `Bearer ${env.FARM_API_TOKEN}`;
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
      if (/\/farms\/[^/]+\/endpoints$/.test(url.pathname)) {
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
      "farmd에 연결할 수 없습니다. 데몬 주소와 서버 token 설정을 확인하세요.",
    ),
  );
  req.on("aborted", () => upstream.destroy());
  res.on("close", () => upstream.destroy());
  req.pipe(upstream);
}
