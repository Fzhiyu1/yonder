# Deploying yonder-relay

One static binary. The relay only forwards Noise ciphertext; it stores nothing but its own
identity key (`relay.key`). Optionally it also serves the web client (`--web-dir`).

## TLS modes

1. Behind a reverse proxy (`--behind-proxy`): listen on localhost, let nginx/Caddy terminate
   TLS and forward WebSocket upgrades (see `nginx-yonder.conf.example`). X-Forwarded-For is
   used for per-IP connection limits.
2. Certificate files (`--tls-cert fullchain.pem --tls-key key.pem`): the relay terminates TLS
   itself and re-reads the files when they change (checked every 60 s), so acme.sh/certbot
   renewals apply without a restart.
3. Automatic Let's Encrypt (`--acme-domain relay.example.com --acme-email you@example.com
   --acme-cache /var/lib/yonder-relay/acme --listen 0.0.0.0:443`): TLS-ALPN-01 challenge, so
   the relay must be reachable on port 443 for that domain. `--acme-staging` for testing.

## systemd

```
install -Dm755 yonder-relay /opt/yonder-relay/yonder-relay
install -Dm644 yonder-relay.service /etc/systemd/system/yonder-relay.service
systemctl daemon-reload && systemctl enable --now yonder-relay
journalctl -u yonder-relay -f
```

`GET /v1/health` returns `ok`; `GET /v1/relay` returns the relay public key and counters.
Memory use is a few MB; the unit caps it at 96 MB.
