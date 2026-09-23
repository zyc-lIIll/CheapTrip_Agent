import { defineConfig, loadEnv } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), "");
  const backend = env.VITE_DEV_BACKEND || "http://127.0.0.1:8080";

  return {
    plugins: [react()],
    server: {
      proxy: {
        "/api": { target: backend, changeOrigin: true },
        "/media": { target: backend, changeOrigin: true },
        "/ws": { target: backend, changeOrigin: true, ws: true },
      },
    },
  };
});
