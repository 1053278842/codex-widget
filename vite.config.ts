import { defineConfig } from 'vite';
import { resolve } from 'node:path';

// Tauri expects a fixed dev port and does not want Vite clearing the Rust logs.
// Two entry points: the capsule itself, and the full-screen completion overlay.
export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    target: 'chrome110',
    minify: 'esbuild',
    sourcemap: false,
    rollupOptions: {
      input: {
        main: resolve(__dirname, 'index.html'),
        overlay: resolve(__dirname, 'overlay.html'),
        'overlay-card': resolve(__dirname, 'overlay-card.html'),
      },
    },
  },
});