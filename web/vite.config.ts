import { defineConfig, loadEnv, type ProxyOptions } from "vite";
import react from "@vitejs/plugin-react";

const apiProxy = (target: string, token: string): ProxyOptions => {
  // changeOrigin rewrites Host to the API, but leaves the browser's Origin
  // pointing at the dev server; the API rejects that mismatch as cross-origin.
  const origin = new URL(target).origin;
  return {
    target,
    changeOrigin: true,
    ws: true,
    configure(proxy) {
      proxy.on("proxyReq", (request) => {
        if (token) request.setHeader("Authorization", `Bearer ${token}`);
        request.setHeader("Origin", origin);
      });
      proxy.on("proxyReqWs", (request) => {
        if (token) request.setHeader("Authorization", `Bearer ${token}`);
        request.setHeader("Origin", origin);
      });
    },
  };
};

export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), "");
  const apiUrl = env.MACHINEEMU_API_URL || "http://127.0.0.1:8000";
  const apiToken = env.MACHINEEMU_API_TOKEN || "";

  return {
    plugins: [react()],
    server: {
      proxy: {
        "/api": apiProxy(apiUrl, apiToken),
        "/ws": apiProxy(apiUrl, apiToken),
      },
    },
  };
});
