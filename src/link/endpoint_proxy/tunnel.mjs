// stackbox tunnel: the MariaDB and RabbitMQ AMQP ports of one stackbox, on this machine's loopback,
// through its relay. Run it with: curl -fsSL <url> | node --input-type=module
// Needs Node.js 22+ (the built-in WebSocket). Nothing to install; the script names no password.
// MYSQL_PORT and AMQP_PORT pick the local ports (default 3306 and 5672); "off" skips one.
import net from "node:net";

if (typeof WebSocket !== "function") {
  console.error("Node.js 22+ is required (built-in WebSocket).");
  process.exit(1);
}

function listenPort(name, fallback) {
  const raw = process.env[name];
  if (raw === undefined) return fallback;
  if (raw === "" || raw === "off") return null;
  const port = Number(raw);
  if (!Number.isInteger(port) || port < 0 || port > 65535) {
    console.error(`${name} must be a port number, or "off" to skip it.`);
    process.exit(1);
  }
  return port;
}

const targets = [
  { name: "MariaDB (MySQL)", url: __MYSQL_URL__, port: listenPort("MYSQL_PORT", 3306) },
  { name: "RabbitMQ (AMQP)", url: __AMQP_URL__, port: listenPort("AMQP_PORT", 5672) },
];

const open = new Set();
const servers = [];

// The TCP side waits for the WebSocket to open, so a client that speaks first (AMQP sends its
// header at once) loses nothing.
function bridge(sock, url) {
  open.add(sock);
  sock.on("close", () => open.delete(sock));
  sock.pause();
  const ws = new WebSocket(url);
  ws.binaryType = "arraybuffer";
  // A socket already closed under the WebSocket must not throw out of the script.
  function closeWs() {
    try {
      ws.close();
    } catch {}
  }
  ws.addEventListener("open", () => sock.resume());
  sock.on("data", (chunk) => {
    if (ws.readyState === WebSocket.OPEN) ws.send(chunk);
  });
  ws.addEventListener("message", (ev) => {
    if (!sock.destroyed) sock.write(Buffer.from(ev.data));
  });
  ws.addEventListener("close", () => sock.destroy());
  ws.addEventListener("error", () => sock.destroy());
  sock.on("close", closeWs);
  sock.on("error", closeWs);
}

for (const target of targets) {
  if (target.port === null) continue;
  const server = net.createServer((sock) => bridge(sock, target.url));
  server.on("error", (error) => {
    console.error(`${target.name}: ${error.message}`);
    process.exit(1);
  });
  servers.push(server);
  server.listen(target.port, "127.0.0.1", () => {
    console.log(`${target.name} -> 127.0.0.1:${server.address().port}`);
  });
}

let stopping = false;
function stop() {
  if (stopping) return;
  stopping = true;
  for (const sock of open) sock.destroy();
  Promise.all(servers.map((s) => new Promise((done) => s.close(() => done())))).then(() =>
    process.exit(0),
  );
}
process.on("SIGINT", stop);
process.on("SIGTERM", stop);
