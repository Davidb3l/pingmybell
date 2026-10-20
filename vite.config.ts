import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

export default defineConfig({
  plugins: [svelte()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    // Vite 8 raised the default target to Safari 16.4 / Chrome 111; the app
    // supports macOS 13+, whose WebKit can be Safari 16.0. Keep Vite 6's old
    // baseline so the bundle does not silently drop those systems.
    target: ["chrome107", "edge107", "firefox104", "safari16"],
    rolldownOptions: {
      input: {
        main: "index.html",
        overlay: "overlay.html",
        reply: "reply.html",
      },
    },
  },
});
