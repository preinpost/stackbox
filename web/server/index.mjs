import { stat } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { createPortalServer } from "./app.mjs";

const root = fileURLToPath(new URL("../dist/", import.meta.url));
await stat(path.join(root, "index.html")).catch(() => {
  throw new Error("먼저 npm run build를 실행하세요.");
});
const server = createPortalServer({ root });
const port = Number(process.env.PORT || 10020);
server.listen(port, process.env.PORTAL_HOST || "127.0.0.1", () =>
  console.log(
    `stackbox portal: http://${process.env.PORTAL_HOST || "127.0.0.1"}:${server.address().port}`,
  ),
);
for (const signal of ["SIGINT", "SIGTERM"])
  process.on(signal, () => {
    server.close();
    server.closeAllConnections();
  });
