## HTTPS Setup

**Caddy** (recommended): Auto-manages SSL certificates. Install, add `valeratrades.com { reverse_proxy localhost:61156 }` to `/etc/caddy/Caddyfile`, reload.

**Nginx + Certbot**: Install nginx and certbot, configure reverse proxy to `127.0.0.1:61156`, run `sudo certbot --nginx -d valeratrades.com`.

## DNS
Add A records for `@` and `www` pointing to your server IP.

## Updating
```bash
cd ~/s/site && git pull && nix build --rebuild && sudo systemctl restart valeratrades
```

---

> **Tip:** If styles appear broken after deployment, try a hard refresh with `Ctrl+Shift+R` to bypass browser cache.
