import { defineConfig, loadEnv } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';
import { fileURLToPath, URL } from 'node:url';

const PROJECT_SPA_PAGES =
  '(?:project|decisions|glossary|tasks|prompts|adr|snapshots|activity|graph|runs|org|settings|status|nodes)';

function isSpaNavigation(pathname: string): boolean {
  return (
    pathname === '/board' ||
    /^\/projects\/[^/]+\/?$/.test(pathname) ||
    new RegExp(`^/projects/[^/]+/${PROJECT_SPA_PAGES}(?:/[^/]+)?/?$`).test(pathname)
  );
}

export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), '');
  const daemon = env.ORGASMIC_DAEMON_URL ?? 'http://127.0.0.1:4848';
  const devToken = env.ORGASMIC_DEV_TOKEN ?? '';
  const appBase = env.ORGASMIC_UI_BASE_PATH || '/';
  const sdkEntries = {
    'plugin-sdk/index': 'src/plugin-sdk/index.ts',
    'plugin-sdk/react': 'src/plugin-sdk/react.ts',
    'plugin-sdk/jsx-runtime': 'src/plugin-sdk/jsx-runtime.ts',
    'plugin-sdk/react-dom-client': 'src/plugin-sdk/react-dom-client.ts',
  };
  const imports = {
    '@orgasmic/plugin-sdk': `${appBase}plugin-sdk/index.js`,
    react: `${appBase}plugin-sdk/react.js`,
    'react/jsx-runtime': `${appBase}plugin-sdk/jsx-runtime.js`,
    'react-dom/client': `${appBase}plugin-sdk/react-dom-client.js`,
  };

  function injectAuth(proxyReq: { setHeader: (name: string, value: string) => void }) {
    if (devToken) proxyReq.setHeader('Authorization', `Bearer ${devToken}`);
  }

  return {
    base: appBase,
    plugins: [
      {
        name: 'orgasmic-plugin-sdk',
        transformIndexHtml: {
          order: 'post',
          handler: () => [{ tag: 'script', attrs: { type: 'importmap' }, children: JSON.stringify({ imports }), injectTo: 'head-prepend' }],
        },
        configureServer(server) {
          server.middlewares.use((req, _res, next) => {
            const path = req.url?.split('?')[0];
            for (const [name, entry] of Object.entries(sdkEntries)) {
              if (path === `${appBase}${name}.js`) req.url = `${appBase}${entry}`;
            }
            next();
          });
        },
      },
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
        '^/(plugins/|prototype-frame.html)': {
          target: daemon,
          changeOrigin: true,
          configure: (proxy) => proxy.on('proxyReq', injectAuth),
        },
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
        input: { app: fileURLToPath(new URL('./index.html', import.meta.url)), ...Object.fromEntries(Object.entries(sdkEntries).map(([name, path]) => [name, fileURLToPath(new URL(`./${path}`, import.meta.url))])) },
        preserveEntrySignatures: 'strict',
        output: {
          entryFileNames: (chunk) => chunk.name.startsWith('plugin-sdk/') ? '[name].js' : 'assets/[name]-[hash].js',
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
