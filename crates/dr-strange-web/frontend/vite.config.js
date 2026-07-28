import { defineConfig } from 'vite'
import { svelte } from '@sveltejs/vite-plugin-svelte'

// The build output is embedded into the drsg binary (rust-embed), so it must
// be fully self-contained. During `bun run dev`, /rpc and /ws proxy to a
// locally-running `drsg serve` (default port 7700).
export default defineConfig({
  plugins: [svelte()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
  server: {
    proxy: {
      '/rpc': 'http://localhost:7700',
      '/ws': { target: 'ws://localhost:7700', ws: true },
    },
  },
})
