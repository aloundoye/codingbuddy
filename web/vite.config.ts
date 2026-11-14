import { defineConfig } from "vite";
import preact from "@preact/preset-vite";

export default defineConfig({
  plugins: [preact()],
  build: {
    outDir: "dist",
    emptyOutDir: true,
  },
  server: {
    proxy: {
      "/rpc": {
        target: "http://127.0.0.1:8199",
        changeOrigin: true,
        configure(proxy) {
          proxy.on("proxyReq", (outbound, inbound) => {
            const origin = inbound.headers.origin;
            if (origin === `http://${inbound.headers.host}`) {
              outbound.setHeader("Origin", "http://127.0.0.1:8199");
            }
          });
        },
      },
      "/health": { target: "http://127.0.0.1:8199", changeOrigin: true },
    },
  },
});
