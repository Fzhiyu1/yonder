// Renders the app icon SVG to PNGs with Playwright's bundled Chromium.
import { mkdirSync } from 'node:fs';
import { chromium } from 'playwright';

const out = new URL('../public/icons/', import.meta.url);
mkdirSync(out, { recursive: true });

// A terminal prompt chevron + cursor bar on a near-black tile.
function svg(size, { maskable = false, rounded = true } = {}) {
  const pad = maskable ? size * 0.2 : 0;
  const inner = size - pad * 2;
  const r = rounded && !maskable ? size * 0.22 : 0;
  const s = inner / 100;
  const x = (v) => pad + v * s;
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${size}" height="${size}" viewBox="0 0 ${size} ${size}">
  <rect width="${size}" height="${size}" rx="${r}" fill="#111113"/>
  <path d="M ${x(24)} ${x(30)} L ${x(48)} ${x(50)} L ${x(24)} ${x(70)}" fill="none" stroke="#ffffff" stroke-width="${9 * s}" stroke-linecap="round" stroke-linejoin="round"/>
  <rect x="${x(56)}" y="${x(62)}" width="${24 * s}" height="${9 * s}" rx="${4.5 * s}" fill="#3b82f6"/>
</svg>`;
}

const targets = [
  { file: 'icon-192.png', size: 192 },
  { file: 'icon-512.png', size: 512 },
  { file: 'icon-maskable-512.png', size: 512, opts: { maskable: true } },
  { file: 'apple-touch-icon.png', size: 180, opts: { rounded: false } },
];

const browser = await chromium.launch();
try {
  const page = await browser.newPage({ deviceScaleFactor: 1 });
  for (const t of targets) {
    await page.setViewportSize({ width: t.size, height: t.size });
    await page.setContent(`<html><body style="margin:0;background:transparent">${svg(t.size, t.opts)}</body></html>`);
    await page.locator('svg').screenshot({ path: new URL(t.file, out).pathname, omitBackground: true });
    console.log('wrote', t.file);
  }
} finally {
  await browser.close();
}
