export function detectSep(path: string, hint?: string): string {
  if (hint) return hint;
  return /^[A-Za-z]:\\/.test(path) || (path.includes('\\') && !path.includes('/')) ? '\\' : '/';
}

export function joinPath(dir: string, name: string, sep = detectSep(dir)): string {
  if (dir.endsWith(sep)) return dir + name;
  return dir + sep + name;
}

export function baseName(path: string, sep = detectSep(path)): string {
  const trimmed = path.length > 1 && path.endsWith(sep) ? path.slice(0, -1) : path;
  const i = trimmed.lastIndexOf(sep);
  return i >= 0 ? trimmed.slice(i + 1) || trimmed : trimmed;
}

export function parentPath(path: string, sep = detectSep(path)): string | null {
  const trimmed = path.length > 1 && path.endsWith(sep) ? path.slice(0, -1) : path;
  const i = trimmed.lastIndexOf(sep);
  if (i < 0) return null;
  if (i === 0) return trimmed === sep ? null : sep;
  const parent = trimmed.slice(0, i);
  if (/^[A-Za-z]:$/.test(parent)) return parent + sep;
  return parent;
}

export interface Crumb {
  name: string;
  path: string;
}

/** Breadcrumb segments; works for `/a/b`, `C:\a\b` and `\\server\share\x`. */
export function breadcrumbs(path: string, sep = detectSep(path)): Crumb[] {
  const out: Crumb[] = [];
  if (sep === '/') {
    out.push({ name: '/', path: '/' });
    let acc = '';
    for (const part of path.split('/').filter(Boolean)) {
      acc += '/' + part;
      out.push({ name: part, path: acc });
    }
    return out;
  }
  const parts = path.split('\\').filter(Boolean);
  if (!parts.length) return out;
  let acc = path.startsWith('\\\\') ? '\\\\' + parts.shift() : (parts.shift() as string);
  out.push({ name: acc, path: /^[A-Za-z]:$/.test(acc) ? acc + '\\' : acc });
  for (const part of parts) {
    acc += '\\' + part;
    out.push({ name: part, path: acc });
  }
  return out;
}
