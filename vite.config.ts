import { defineConfig } from 'vite';

// Tauri expects a fixed dev port and does not want Vite clearing the Rust logs.
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
  },
});