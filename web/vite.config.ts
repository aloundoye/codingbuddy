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
      "/rpc": "http://127.0.0.1:8199",
      "/health": "http://127.0.0.1:8199",
    },
  },
});
