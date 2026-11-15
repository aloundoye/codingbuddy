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
            const hosts = inbound.headersDistinct.host ?? [];
            const origins = inbound.headersDistinct.origin ?? [];
            if (hosts.length === 1 && origins.length === 1 && origins[0] === `http://${hosts[0]}`) {
              outbound.setHeader("Origin", "http://127.0.0.1:8199");
            } else if (hosts.length !== 1 || origins.length > 0) {
              outbound.setHeader("Origin", "null");
            }
          });
        },
      },
      "/health": { target: "http://127.0.0.1:8199", changeOrigin: true },
    },
  },
});
