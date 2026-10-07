import type { ChatItem } from '../proto/generated/ChatItem';

/** Something in a chat that can be opened in the viewer. */
export type ArtifactKind = 'image' | 'pdf' | 'html' | 'web' | 'text';

export interface Artifact {
  /** Absolute host path, or a loopback URL for `web`. */
  ref: string;
  kind: ArtifactKind;
  /** Short label (file name, or host:port/path). */
  name: string;
}

const IMAGE = /\.(png|jpe?g|gif|webp|svg|bmp|avif|ico)$/i;
const PDF = /\.pdf$/i;
const HTML = /\.html?$/i;
const TEXT = /\.(md|markdown|txt|log|csv|json|ya?ml|toml)$/i;

export function baseOf(path: string): string {
  return path.split(/[\\/]/).filter(Boolean).pop() ?? path;
}

/** Kind of a host path by its extension; undefined when the viewer cannot show it. */
export function kindOfPath(path: string): ArtifactKind | undefined {
  if (IMAGE.test(path)) return 'image';
  if (PDF.test(path)) return 'pdf';
  if (HTML.test(path)) return 'html';
  if (TEXT.test(path)) return 'text';
  return undefined;
}

const LOOPBACK = /^http:\/\/(localhost|127\.0\.0\.1|\[::1\])(:\d+)?(\/[^\s)\]>"'`]*)?/i;

export function isLoopbackUrl(url: string): boolean {
  return LOOPBACK.test(url);
}

export function artifactFor(ref: string): Artifact | undefined {
  if (isLoopbackUrl(ref)) {
    const u = new URL(ref);
    return { ref: u.href, kind: 'web', name: `${u.host}${u.pathname === '/' ? '' : u.pathname}` };
  }
  const kind = kindOfPath(ref);
  return kind ? { ref, kind, name: baseOf(ref) } : undefined;
}

function isAbsolute(p: string): boolean {
  return p.startsWith('/') || /^[A-Za-z]:[\\/]/.test(p) || p.startsWith('\\\\') || /^https?:\/\//i.test(p);
}

/** Make a path an agent reported relative to its working directory absolute. */
export function resolveRef(ref: string, cwd?: string): string {
  if (!cwd || isAbsolute(ref) || ref.startsWith('~')) return ref;
  const sep = /^[A-Za-z]:\\/.test(cwd) ? '\\' : '/';
  const rel = ref.replace(/^\.[\\/]/, '').replace(/[\\/]/g, sep);
  return cwd.endsWith(sep) ? cwd + rel : cwd + sep + rel;
}

// Absolute paths in prose: Unix (`/Users/a/b.png`, `~/x.pdf`) and Windows (`C:\x\y.html`).
const PATH_IN_TEXT = /(?:^|[\s(（「"'`[<：:，,、])((?:~|\/)[^\s)）」"'`\]>，。；：]*\.[A-Za-z0-9]{2,8}|[A-Za-z]:\\[^\s)）」"'`\]>，。；：]*\.[A-Za-z0-9]{2,8})/g;
const URL_IN_TEXT = /http:\/\/(?:localhost|127\.0\.0\.1|\[::1\])(?::\d+)?(?:\/[^\s)\]>"'`，。）」]*)?/gi;
// Relative paths in inline code (`docs/report.html`), only with a viewable extension.
const REL_IN_CODE = /`((?:\.{0,2}[\\/])?[\w.@-]+(?:[\\/][\w.@ -]+)*\.[A-Za-z0-9]{2,8})`/g;

/** Paths and loopback URLs mentioned in a message, in order, without duplicates. */
export function refsInText(text: string, home?: string): string[] {
  const out: string[] = [];
  for (const m of text.matchAll(URL_IN_TEXT)) out.push(m[0].replace(/[.,;:]+$/, ''));
  for (const m of text.matchAll(PATH_IN_TEXT)) {
    let p = m[1].replace(/[.,;:]+$/, '');
    if (p.startsWith('~') && home) p = home.replace(/[\\/]$/, '') + p.slice(1);
    out.push(p);
  }
  for (const m of text.matchAll(REL_IN_CODE)) {
    if (!m[1].startsWith('/') && !m[1].startsWith('\\') && kindOfPath(m[1])) out.push(m[1]);
  }
  return [...new Set(out)];
}

/** Viewable artifacts of one chat item; relative paths resolve against `cwd`. */
export function itemArtifacts(item: ChatItem, home?: string, cwd?: string): Artifact[] {
  const refs: string[] = [];
  switch (item.kind) {
    case 'tool':
    case 'file_change':
    case 'user':
      refs.push(...item.paths);
      break;
    case 'agent':
      refs.push(...refsInText(item.text ?? '', home));
      break;
    default:
      break;
  }
  const out: Artifact[] = [];
  const seen = new Set<string>();
  for (const r of refs) {
    const a = artifactFor(resolveRef(r, cwd));
    if (a && !seen.has(a.ref)) {
      seen.add(a.ref);
      out.push(a);
    }
  }
  return out;
}

/** Every artifact of a chat, newest first. */
export function chatArtifacts(items: ChatItem[], home?: string, cwd?: string): Artifact[] {
  const seen = new Set<string>();
  const out: Artifact[] = [];
  for (let i = items.length - 1; i >= 0; i--) {
    for (const a of itemArtifacts(items[i], home, cwd)) {
      if (seen.has(a.ref)) continue;
      seen.add(a.ref);
      out.push(a);
    }
  }
  return out;
}

/**
 * Asset references of an HTML page that belong to the same server: `src`/`href` of scripts,
 * styles, images, icons. Returns absolute URLs.
 */
export function sameOriginAssets(html: string, pageUrl: string): string[] {
  const base = new URL(pageUrl);
  const out = new Set<string>();
  const re = /<(script|link|img|source|video|audio)\b[^>]*?\s(src|href)\s*=\s*["']([^"']+)["']/gi;
  for (const m of html.matchAll(re)) {
    const tag = m[1].toLowerCase();
    if (tag === 'link' && !/rel\s*=\s*["'][^"']*(stylesheet|icon|modulepreload|preload)/i.test(m[0])) continue;
    try {
      const u = new URL(m[3], base);
      if (u.origin === base.origin) out.add(u.href);
    } catch {
      // ignore malformed
    }
  }
  return [...out];
}
