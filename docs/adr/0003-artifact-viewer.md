# 0003 Artifact viewer: images, PDFs, HTML and web pages from a chat, on the phone

Date: 2026-10-05. Status: accepted.

## Context

Agents produce things worth looking at: screenshots they read (`view_image`), images they
generate, PDFs, HTML reports, and web apps served by a dev server on the host
(`http://localhost:5173`). The chat only showed a line like "查看图片" with a path; a phone
could not open any of it. The relay must never see plaintext, so the phone cannot simply load
`https://relay/…/file.pdf`.

## Decision

1. **Everything goes through the encrypted channel.** Files are read with the existing
   `fs_read` (chunked, permission `files`). Web pages from the host's own dev servers come
   through a new `http_fetch{url}`: GET only, loopback hosts only (`localhost`, `127.0.0.1`,
   `[::1]`), no redirects, 20 MiB cap. The relay keeps seeing ciphertext only.
2. **Artifacts are found in the chat, not declared.** The client scans chat items for
   previewable things: image paths of `view_image` / image generation, file-change paths with a
   previewable extension, absolute paths and `http://localhost:…` links in agent messages. Each
   becomes a chip or thumbnail in place, and the session collects them in an "产物" list.
3. **One viewer, stacked windows.** Opening an artifact opens a full-height viewer above the
   chat (a sheet on phones, a side panel on wide screens) with tabs: several artifacts stay open
   at once and the chat keeps running underneath. Closing the viewer returns to the same scroll
   position.
4. **Renderers per type.**
   - Images: `<img>` from a Blob URL, pinch / double-tap zoom.
   - PDF: rendered with pdf.js (lazy chunk; cmaps, standard fonts and wasm decoders under
     `/pdfjs/`), every page fit to width, pages drawn near the viewport only (canvases capped
     for iOS), pinch / button zoom, page counter. An iframe with a Blob URL shows only the
     first page on iOS Safari, so it is not used. "Save" hands the file to the system share
     sheet.
   - Web page (`http://localhost:port/…`) and HTML file: rendered on the **preview origin**
     (point 5). The page runs as a real page with its own origin, so ES modules, CSS, images,
     fonts, `fetch('/api/…')` and in-page links work. A dev server is reached with `http_fetch`;
     an HTML file and its relative assets are read with `fs_read` from the file's directory.
   - Fallback when the preview origin is not configured, the browser has no service worker, or
     the bridge does not report ready within 6 s: the page markup in a sandboxed iframe
     (`sandbox="allow-scripts"`, `srcdoc`), without its assets.
   - Text / code / Markdown: text view (Markdown rendered).
5. **Preview origin with a service worker.** A second origin on the relay server
   (`https://relay.example.com:2099`, same IP certificate as the relay) serves exactly two static
   files: `/__preview/bridge.html` and `/preview-sw.js` (source: `web/preview/`). Flow:
   - The app registers a preview with a random 128-bit id and embeds
     `bridge.html?id=<id>&parent=<app origin>&path=<path>&mode=root|prefix`.
   - The bridge registers the worker (scope `/`), reports `yonder-preview-ready`, and opens
     the page in a nested frame: dev servers at their real path (`mode=root`, absolute asset
     URLs keep working), files under `/__p/<id>/` (`mode=prefix`, so several file previews
     cannot collide).
   - The worker intercepts every request of that frame: by the `/__p/<id>/` prefix, by
     `clientId` (remembered from `resultingClientId`, mirrored in Cache Storage because idle
     workers are stopped) or, for navigations, by the referrer. It asks the bridge through a
     `MessagePort`; the bridge asks the app; the app answers with status, content type and body.
     Only GET/HEAD; others get 405. Same-server redirects stay inside the preview.
   - The relay server never sees page content: it only hands out the two static files.

   Guards: the bridge only talks to the origin in `parent`, which must equal
   `location.ancestorOrigins[0]` where the browser has that API, and only relays messages from
   its service worker. The app only answers messages whose `origin` is the preview origin, for
   ids it registered and has not disposed, and only inside the source: an `http` preview can
   only reach its own loopback origin; a file preview only paths under its directory, never
   `..`, never a segment starting with `.` (`.git`, `.env`), and only viewer-safe extensions
   (html, css, js, json, images, fonts, media, txt, map, wasm). At most 6 host requests per
   preview run at once.

   Pages from different previews share the preview origin (storage, cookies). They are the
   user's own pages from their own machines; the app origin with its keys stays separate.

## Consequences

- Viewing a large PDF moves the whole file over the relay (in 1 MiB encrypted chunks).
- Dev-server pages: GET requests to their own server work, including runtime `fetch`.
  WebSockets (HMR live reload), POST requests and other servers' APIs do not; reload the
  preview to see changes.
- Every page request is a round trip over the relay, so pages with hundreds of modules (an
  unbundled Vite dev server) take seconds to load. A built site (`vite build`, opened as a
  file) loads faster.
- The preview origin is configured at build time (`VITE_PREVIEW_ORIGIN`), overridable per
  browser with `localStorage['yonder.previewOrigin']`. Self-hosters without a second port
  get the `srcdoc` fallback.
- Phones without the `files` permission see the chips but cannot open them.
