import { defineConfig, loadEnv } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';
import { fileURLToPath, URL } from 'node:url';

const PROJECT_SPA_PAGES =
  '(?:project|decisions|architecture|glossary|tasks|prompts|adr|snapshots|activity|graph|runs|org|settings|status)';

function isSpaNavigation(pathname: string): boolean {
  return (
    pathname === '/board' ||
    /^\/projects\/[^/]+\/?$/.test(pathname) ||
    new RegExp(`^/projects/[^/]+/${PROJECT_SPA_PAGES}(?:/[^/]+)?/?$`).test(pathname)
  );
}

export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), '');
  const daemon = env.ORGASMIC_DAEMON_URL ?? 'http://127.0.0.1:8739';
  const devToken = env.ORGASMIC_DEV_TOKEN ?? '';
  const appBase = env.ORGASMIC_UI_BASE_PATH || '/';

  function injectAuth(proxyReq: { setHeader: (name: string, value: string) => void }) {
    if (devToken) proxyReq.setHeader('Authorization', `Bearer ${devToken}`);
  }

  return {
    base: appBase,
    plugins: [
      {
        name: 'orgasmic-spa-navigation-fallback',
        configureServer(server) {
          server.middlewares.use((req, _res, next) => {
            const pathname = req.url?.split('?')[0] ?? '';
            const accept = req.headers.accept ?? '';
            if (req.method === 'GET' && accept.includes('text/html') && isSpaNavigation(pathname)) {
              req.url = '/index.html';
            }
            next();
          });
        },
      },
      react(),
      tailwindcss(),
    ],
    resolve: {
      alias: {
        '@': fileURLToPath(new URL('./src', import.meta.url)),
      },
    },
    server: {
      allowedHosts: ['orgasmic.trydev.app'],
      proxy: {
        '^/api': {
          target: daemon,
          changeOrigin: true,
          ws: true,
          configure: (proxy) => {
            proxy.on('proxyReq', (proxyReq) => {
              injectAuth(proxyReq);
            });
            proxy.on('proxyReqWs', (proxyReq) => {
              injectAuth(proxyReq);
            });
          },
        },
      },
    },
    build: {
      rollupOptions: {
        output: {
          manualChunks(id) {
            if (!id.includes('node_modules')) return undefined;
            if (id.includes('/node_modules/@tanstack/')) return 'vendor-tanstack';
            if (
              id.includes('/node_modules/@codemirror/') ||
              id.includes('/node_modules/codemirror/')
            ) {
              return 'vendor-codemirror';
            }
            if (id.includes('/node_modules/radix-ui/')) return 'vendor-radix';
            if (id.includes('/node_modules/lucide-react/')) return 'vendor-lucide';
            if (id.includes('/node_modules/@xterm/')) return 'vendor-xterm';
            return undefined;
          },
        },
      },
    },
  };
});
