/// <reference types="vitest/config" />
import { defineConfig, type Plugin } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';
import { cpSync, createReadStream, existsSync, readFileSync, statSync } from 'node:fs';
import { dirname, join, normalize, resolve, sep } from 'node:path';
import { createRequire } from 'node:module';

const pkg = JSON.parse(readFileSync(new URL('./package.json', import.meta.url), 'utf8')) as { version: string };

/**
 * pdf.js fetches CMaps (CJK text without embedded fonts), standard fonts, wasm image decoders and
 * ICC profiles at runtime. Serve them from node_modules in dev and copy them to dist/pdfjs on build.
 */
function pdfjsAssets(): Plugin {
  const root = dirname(createRequire(import.meta.url).resolve('pdfjs-dist/package.json'));
  const dirs = ['cmaps', 'standard_fonts', 'wasm', 'iccs'];
  let outDir = 'dist';
  return {
    name: 'yonder-pdfjs-assets',
    configResolved(c) {
      outDir = resolve(c.root, c.build.outDir);
    },
    configureServer(server) {
      server.middlewares.use('/pdfjs', (req, res, next) => {
        const rel = normalize(decodeURIComponent((req.url ?? '').split('?')[0])).replace(/^[/\\]+/, '');
        const file = join(root, rel);
        if (!dirs.includes(rel.split(sep)[0]) || !file.startsWith(root + sep) || !existsSync(file) || !statSync(file).isFile()) return next();
        if (file.endsWith('.wasm')) res.setHeader('Content-Type', 'application/wasm');
        createReadStream(file).pipe(res);
      });
    },
    writeBundle() {
      for (const d of dirs) cpSync(join(root, d), join(outDir, 'pdfjs', d), { recursive: true });
    },
  };
}

export default defineConfig({
  base: '/',
  plugins: [react(), tailwindcss(), pdfjsAssets()],
  define: {
    __APP_VERSION__: JSON.stringify(pkg.version),
  },
  build: {
    outDir: 'dist',
    target: 'es2022',
    chunkSizeWarningLimit: 1200,
  },
  server: {
    host: true,
  },
  test: {
    environment: 'node',
    include: ['src/**/*.test.ts'],
  },
});
