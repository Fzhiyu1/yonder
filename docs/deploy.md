# Deploying yonder

Three pieces: the relay (one small server with a public address), the web client (static
files, served by the relay), and one host daemon per machine you want to reach.

## Build

```sh
cd web && pnpm install && VITE_PREVIEW_ORIGIN=https://relay.example.com:2099 pnpm build   # -> web/dist
cargo build -p yonder-relay --release           # relay (Linux: cargo zigbuild --target x86_64-unknown-linux-musl)
cargo build -p yonder-cli --release             # host daemon + CLI for this machine
cargo zigbuild -p yonder-cli --release --target x86_64-unknown-linux-musl   # Linux hosts
cargo build -p yonder-cli --release --target x86_64-pc-windows-gnu          # Windows hosts
```

## Relay and web client

See deploy/relay/README.md for TLS modes and the systemd unit. The web client is served
by the relay from `--web-dir`; replace the directory atomically:

```sh
COPYFILE_DISABLE=1 tar -C web/dist -czf /tmp/web.tgz .
scp /tmp/web.tgz relay:/tmp/
ssh relay 'set -e; ts=$(date +%s); mkdir -p /opt/yonder-relay/web.new /opt/yonder-relay/backup
  tar -C /opt/yonder-relay/web.new -xzf /tmp/web.tgz; chmod -R a+rX /opt/yonder-relay/web.new
  mv /opt/yonder-relay/web /opt/yonder-relay/backup/web-$ts; mv /opt/yonder-relay/web.new /opt/yonder-relay/web'
```

The service worker fetches navigations network-first, so clients pick up a new build on the
next load. `index.html` is served with `Cache-Control: no-cache`; assets are content-hashed.

## Preview origin (web pages and HTML files in the artifact viewer)

See docs/adr/0003-artifact-viewer.md. A second HTTPS origin serves two static files from
`web/preview/` (`/__preview/bridge.html`, `/preview-sw.js`); page content itself goes through
the encrypted channel. The app learns the origin at build time from `VITE_PREVIEW_ORIGIN` (not
a secret; a browser can override it with `localStorage['yonder.previewOrigin']`). Without it,
or when the bridge does not start, the viewer falls back to a sandboxed frame without assets.

On the relay host it is nginx on port 2099 with the relay's IP certificate,
`/etc/nginx/conf.d/yonder-preview-2099.conf`:

```nginx
server {
    listen 2099 ssl;
    server_name relay.example.com;
    ssl_certificate     /etc/nginx/sing-cert/ip-fullchain.pem;
    ssl_certificate_key /etc/nginx/sing-cert/ip-key.pem;
    ssl_protocols TLSv1.2 TLSv1.3;
    root /opt/yonder-relay/preview;
    access_log off;
    add_header X-Content-Type-Options nosniff always;
    add_header Referrer-Policy same-origin always;
    location = /preview-sw.js {
        default_type text/javascript;
        add_header Cache-Control "no-store" always;
        add_header Service-Worker-Allowed "/" always;
        add_header X-Content-Type-Options nosniff always;
        try_files $uri =404;
    }
    location = /__preview/bridge.html {
        default_type text/html;
        add_header Cache-Control "no-store" always;
        add_header X-Content-Type-Options nosniff always;
        add_header Referrer-Policy same-origin always;
        try_files $uri =404;
    }
    location / { return 404; }
}
```

The preview origin must differ from the app origin (another port is enough) and must not
serve anything else: every page on it can read what the others store. Update the files
independently of the app:

```sh
COPYFILE_DISABLE=1 tar --no-xattrs -C web/preview -czf /tmp/preview.tgz .
scp /tmp/preview.tgz relay:/tmp/
ssh relay 'set -e; ts=$(date +%s); mkdir -p /opt/yonder-relay/preview.new /opt/yonder-relay/backup
  tar -C /opt/yonder-relay/preview.new --no-same-owner -xzf /tmp/preview.tgz; chmod -R a+rX /opt/yonder-relay/preview.new
  [ -d /opt/yonder-relay/preview ] && mv /opt/yonder-relay/preview /opt/yonder-relay/backup/preview-$ts
  mv /opt/yonder-relay/preview.new /opt/yonder-relay/preview'
curl -sI https://relay.example.com:2099/preview-sw.js   # expect 200, Service-Worker-Allowed: /
```

Both files are `no-store`, and the bridge registers the worker with `updateViaCache: 'none'`,
so open previews pick up a new worker on the next preview.

## Host daemon

Per machine, as the user who runs the agents (no admin rights needed):

```sh
yonder init --name <display name> [--relay wss://host:port/v1/ws] [--root <folder>...]
yonder service install    # macOS launchd agent / Linux systemd --user unit / Windows logon task
yonder status             # expect "relay: ... (connected)"
yonder pair               # QR code + link; open it on the phone or browser
```

- The default relay is compiled in (`DEFAULT_RELAY` in crates/yonder-host/src/config.rs).
- `fs_roots` (default: home) limits the file manager; `fs_deny` blocks paths inside them.
- Agents are found on the user's PATH plus common install dirs; override per agent in
  `config.toml` with `[agents.codex] program = ["/path/to/codex"]` and `env = {...}`.
- Linux: `loginctl enable-linger <user>` keeps the daemon running while logged out.
- Windows: the logon task starts `wscript.exe <data>\yonder-daemon.vbs` (no console window).

### Upgrading a host

Sessions run in their own supervisor processes and survive a daemon restart; the new
daemon re-adopts them. Replace the binary, then restart the service:

- macOS: copy over `~/.local/bin/yonder`, `codesign -s - -f` it, `yonder service install`.
- Linux: `install -m755 yonder ~/.local/bin/yonder.new && mv ~/.local/bin/yonder.new ~/.local/bin/yonder`,
  then `systemctl --user restart yonder`.
- Windows: running supervisors keep `yonder.exe` open, so it cannot be overwritten. Stop the
  daemon (`yonder stop`), rename the old exe (`yonder.old-<ts>.exe`), copy the new one, run
  `yonder service install`. Old copies can be deleted once their sessions have ended.

### Removing

`yonder service uninstall`, then delete the binary, the config dir (`yonder paths`) and
the data dir. `yonder devices revoke <device>` removes a paired phone or browser.

## Pairing a phone

Run `yonder pair` on the host and scan the QR code with the phone camera, or open the
printed link. Check that the fingerprint shown in the app matches the host. On iOS, add the
page to the Home Screen (Share, Add to Home Screen) to get notifications; then enable them
in Settings, Notifications.
