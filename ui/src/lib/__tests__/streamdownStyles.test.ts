import { mkdtempSync, readdirSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';
import { build } from 'vite';

const uiRoot = fileURLToPath(new URL('../../../', import.meta.url));

describe('Streamdown production CSS', () => {
  it(
    'includes dependency-only wrapping utilities and KaTeX rules',
    async () => {
      const outDir = mkdtempSync(join(tmpdir(), 'orgasmic-streamdown-css-'));

      try {
        await build({
          root: uiRoot,
          configFile: join(uiRoot, 'vite.config.ts'),
          logLevel: 'silent',
          build: { emptyOutDir: true, outDir },
        });

        const assetsDir = join(outDir, 'assets');
        const productionCss = readdirSync(assetsDir)
          .filter((name) => name.endsWith('.css'))
          .map((name) => readFileSync(join(assetsDir, name), 'utf8'))
          .join('\n');

        expect(productionCss).toContain('.wrap-anywhere');
        expect(productionCss).toContain('.katex');
      } finally {
        rmSync(outDir, { force: true, recursive: true });
      }
    },
    60_000,
  );
});
