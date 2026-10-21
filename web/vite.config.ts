import { defineConfig, loadEnv } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { proxyApi } from "./server/proxy.mjs";

export default defineConfig(({ mode }) => {
  // These are server-only values; never use a VITE_ prefix for the API token.
  const env = { ...loadEnv(mode, process.cwd(), ""), ...process.env };
  return {
    plugins: [
      react(),
      tailwindcss(),
      {
        name: "farm-api",
        configureServer(server) {
          server.middlewares.use((req, res, next) => {
            if (req.url?.startsWith("/api/")) proxyApi(req, res, env);
            else next();
          });
        },
      },
    ],
    server: { port: 5180, strictPort: true },
    test: {
      environment: "jsdom",
      include: ["src/**/*.test.{ts,tsx}"],
      restoreMocks: true,
    },
  };
});
