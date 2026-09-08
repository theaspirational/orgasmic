import { defineConfig } from 'vitest/config';
import { fileURLToPath, URL } from 'node:url';

export default defineConfig({
  test: {
    environment: 'node',
  },
  resolve: {
    dedupe: ['react', 'react-dom'],
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
      '@orgasmic/plugin-sdk': fileURLToPath(new URL('./src/plugin-sdk/index.ts', import.meta.url)),
    },
  },
});
