import { useSyncExternalStore } from 'react';

export type Route =
  | { name: 'home' }
  | { name: 'session'; host: string; session: string }
  | { name: 'files'; host: string; path?: string }
  | { name: 'host'; host: string }
  | { name: 'history'; host: string }
  | { name: 'settings' };

export function parseHash(hash: string): Route {
  const h = hash.replace(/^#/, '');
  if (h.startsWith('pair=')) return { name: 'home' };
  const [pathPart, query = ''] = h.split('?');
  const parts = pathPart.split('/').filter(Boolean).map(decodeURIComponent);
  if (parts[0] === 'settings') return { name: 'settings' };
  if (parts[0] === 'h' && parts[1]) {
    const host = parts[1];
    if (parts[2] === 's' && parts[3]) return { name: 'session', host, session: parts[3] };
    if (parts[2] === 'files') {
      const path = new URLSearchParams(query).get('path') ?? undefined;
      return { name: 'files', host, path };
    }
    if (parts[2] === 'info') return { name: 'host', host };
    if (parts[2] === 'history') return { name: 'history', host };
  }
  return { name: 'home' };
}

export function routeHash(r: Route): string {
  switch (r.name) {
    case 'home':
      return '#/';
    case 'settings':
      return '#/settings';
    case 'session':
      return `#/h/${encodeURIComponent(r.host)}/s/${encodeURIComponent(r.session)}`;
    case 'files':
      return `#/h/${encodeURIComponent(r.host)}/files${r.path ? `?path=${encodeURIComponent(r.path)}` : ''}`;
    case 'host':
      return `#/h/${encodeURIComponent(r.host)}/info`;
    case 'history':
      return `#/h/${encodeURIComponent(r.host)}/history`;
  }
}

export function navigate(r: Route, replace = false): void {
  const hash = routeHash(r);
  if (location.hash === hash) return;
  if (replace) {
    history.replaceState(null, '', hash);
    window.dispatchEvent(new HashChangeEvent('hashchange'));
  } else {
    location.hash = hash;
  }
}

export function navigateUrl(url: string): void {
  const i = url.indexOf('#');
  if (i >= 0) location.hash = url.slice(i);
}

function subscribe(cb: () => void) {
  window.addEventListener('hashchange', cb);
  return () => window.removeEventListener('hashchange', cb);
}

let cachedHash = '';
let cachedRoute: Route = { name: 'home' };

function snapshot(): Route {
  if (location.hash !== cachedHash) {
    cachedHash = location.hash;
    cachedRoute = parseHash(cachedHash);
  }
  return cachedRoute;
}

export function useRoute(): Route {
  return useSyncExternalStore(subscribe, snapshot, () => cachedRoute);
}
