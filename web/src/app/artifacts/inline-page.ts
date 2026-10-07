import { bytesToB64 } from '../../lib/base64';

/** Fetches a URL of the page's own server (absolute, already resolved). */
export type AssetGetter = (url: string) => Promise<{ status: number; type: string; body: ArrayBuffer } | undefined>;

const MAX_ASSETS = 60;

/**
 * Fallback rendering of a page without the preview origin: the HTML with its same-server
 * stylesheets, classic scripts and images inlined, for a sandboxed `srcdoc` iframe. ES modules
 * with imports and runtime fetches do not work this way; the preview origin handles those.
 */
export async function inlinePage(html: string, pageUrl: string, get: AssetGetter): Promise<string> {
  const doc = new DOMParser().parseFromString(html, 'text/html');
  const base = new URL(pageUrl);
  const same = (ref: string | null) => {
    if (!ref || ref.startsWith('data:') || ref.startsWith('#')) return undefined;
    try {
      const u = new URL(ref, base);
      return u.origin === base.origin ? u.href : undefined;
    } catch {
      return undefined;
    }
  };
  let budget = MAX_ASSETS;
  const fetchOk = async (url: string) => {
    if (budget-- <= 0) return undefined;
    const r = await get(url).catch(() => undefined);
    return r && r.status < 400 ? r : undefined;
  };
  const jobs: Promise<void>[] = [];
  for (const link of Array.from(doc.querySelectorAll('link[rel~="stylesheet"][href]'))) {
    const url = same(link.getAttribute('href'));
    if (!url) continue;
    jobs.push(
      fetchOk(url).then((r) => {
        if (!r) return;
        const style = doc.createElement('style');
        style.textContent = new TextDecoder().decode(r.body);
        link.replaceWith(style);
      }),
    );
  }
  for (const s of Array.from(doc.querySelectorAll('script[src]'))) {
    const url = same(s.getAttribute('src'));
    if (!url) continue;
    jobs.push(
      fetchOk(url).then((r) => {
        if (!r) return;
        s.removeAttribute('src');
        s.textContent = new TextDecoder().decode(r.body);
      }),
    );
  }
  for (const img of Array.from(doc.querySelectorAll('img[src], source[src], video[poster]'))) {
    const attr = img.hasAttribute('src') ? 'src' : 'poster';
    const url = same(img.getAttribute(attr));
    if (!url) continue;
    jobs.push(
      fetchOk(url).then((r) => {
        if (r) img.setAttribute(attr, `data:${r.type};base64,${bytesToB64(new Uint8Array(r.body))}`);
      }),
    );
  }
  await Promise.all(jobs);
  // Links inside the page cannot navigate anywhere useful from a srcdoc frame.
  const meta = doc.createElement('meta');
  meta.setAttribute('name', 'referrer');
  meta.setAttribute('content', 'no-referrer');
  doc.head.prepend(meta);
  return '<!doctype html>\n' + doc.documentElement.outerHTML;
}
