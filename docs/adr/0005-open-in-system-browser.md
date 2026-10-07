# 0005 Open host pages in the phone's own browser

Date: 2026-10-06. Status: accepted.

## Context

The artifact viewer (ADR 0003) shows a host's dev-server page (`http://localhost:5173`) inside
Yonder by fetching every request over the encrypted channel. Users also want to open the same
page in Safari: real WebSockets/HMR, POST, devtools, sharing the tab. Safari on the phone cannot
reach `localhost` of another machine, and has no keys for the relay channel. The relay must
never see plaintext, so proxying through it is not an option.

## Decision

- New request `tailnet_url{url}` (needs `files`): the host rewrites the loopback URL to its own
  Tailscale IPv4 (the source address the OS routes Tailscale's MagicDNS address 100.100.100.100 from) and probes the port there for 2 s.
  It answers `tailnet_url{url, reachable}`, or `not_found` without a Tailscale address.
- The viewer header gets a "more" menu instead of the single copy/save button:
  - web pages: open in browser (via Tailscale), copy the Tailscale address, copy the original
    address. The address is looked up when the page is shown, because iOS only opens a new tab
    synchronously inside the tap.
  - files: share or save (system share sheet on iOS, "open in" other apps), copy path.
- When the port does not answer on the Tailscale address (dev server bound to 127.0.0.1) the
  item is still offered with a hint to listen on 0.0.0.0; the browser shows the failure.
- Public links (`pub`, tunnels) are not offered: they would expose the page to anyone.
- Default tap behaviour is unchanged: public links go to the browser, host-local pages and
  files open in the viewer.

## Consequences

- Works only when the phone is on the same tailnet. Traffic goes phone -> host directly over
  WireGuard; the relay is not involved.
- Many dev servers listen on loopback only; the user has to restart them with `--host`.
- Other overlays use 100.64.0.0/10 too (NetBird on the Mac), so the address is picked by routing to 100.100.100.100, not by range. The Tailscale CLI is not used: the macOS app's CLI cannot start from a launchd agent. Fallback: an interface named like Tailscale, or the only address in the range.
