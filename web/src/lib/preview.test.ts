import { describe, expect, it } from 'vitest';
import { previewTypeOf, resolveFsPath, resolveHttpUrl } from './preview';

describe('preview path guards', () => {
  it('maps file paths under the root', () => {
    expect(resolveFsPath('/Users/a/site', '/', '/index.html')).toBe('/Users/a/site/index.html');
    expect(resolveFsPath('/Users/a/site/', '/', '/css/app.css?v=2')).toBe('/Users/a/site/css/app.css');
    expect(resolveFsPath('/Users/a/site', '/', '/docs/')).toBe('/Users/a/site/docs/index.html');
    expect(resolveFsPath('C:\\w\\site', '\\', '/img/%E5%9B%BE.png')).toBe('C:\\w\\site\\img\\图.png');
  });

  it('rejects escapes, hidden files and other types', () => {
    expect(resolveFsPath('/r', '/', '/../etc/passwd.txt')).toBeUndefined();
    expect(resolveFsPath('/r', '/', '/a/%2e%2e/b.html')).toBeUndefined();
    expect(resolveFsPath('/r', '/', '/a/..%2Fb.html')).toBeUndefined();
    expect(resolveFsPath('/r', '/', '/.git/config.txt')).toBeUndefined();
    expect(resolveFsPath('/r', '/', '/.env')).toBeUndefined();
    expect(resolveFsPath('/r', '\\', '/a%5Cb.html')).toBeUndefined();
    expect(resolveFsPath('/r', '/', '//x.html')).toBeUndefined();
    expect(resolveFsPath('/r', '/', '/id_rsa')).toBeUndefined();
    expect(resolveFsPath('/r', '/', '/notes.md')).toBeUndefined();
    expect(resolveFsPath('/r', '/', '/bad%zz.html')).toBeUndefined();
  });

  it('keeps http requests on the previewed server', () => {
    expect(resolveHttpUrl('http://localhost:5173', '/src/main.ts?t=1')).toBe('http://localhost:5173/src/main.ts?t=1');
    expect(resolveHttpUrl('http://localhost:5173', '//evil.example/x')).toBeUndefined();
    expect(resolveHttpUrl('http://localhost:5173', 'x')).toBeUndefined();
  });

  it('types files by extension', () => {
    expect(previewTypeOf('a/b.MJS')).toMatch(/^text\/javascript/);
    expect(previewTypeOf('x.wasm')).toBe('application/wasm');
    expect(previewTypeOf('Makefile')).toBeUndefined();
  });
});
