# nginx reverse proxy for atv.christopherrutherford.net

This exposes the running Awesome TV app at:

```text
https://atv.christopherrutherford.net
```

and proxies to:

```text
http://127.0.0.1:8409
```

## Prerequisites

- DNS A/AAAA record for `atv.christopherrutherford.net` pointing at the public
  address.
- Router forwards TCP 80 and 443 to this host.
- nginx and certbot installed.
- Awesome TV listening on `0.0.0.0:8409` (the current `.env` configuration).

## 1. Bootstrap HTTP and obtain a certificate

```sh
sudo apt install nginx certbot
sudo mkdir -p /var/www/certbot
sudo cp deploy/nginx/atv.christopherrutherford.net.http-only.conf \
  /etc/nginx/sites-available/atv.christopherrutherford.net
sudo ln -sf /etc/nginx/sites-available/atv.christopherrutherford.net \
  /etc/nginx/sites-enabled/atv.christopherrutherford.net
sudo nginx -t
sudo systemctl reload nginx
```

Verify HTTP first:

```sh
curl -I http://atv.christopherrutherford.net/healthz
```

Then issue the certificate:

```sh
sudo certbot certonly --webroot \
  -w /var/www/certbot \
  -d atv.christopherrutherford.net
```

## 2. Enable the TLS config

```sh
sudo cp deploy/nginx/atv.christopherrutherford.net.conf \
  /etc/nginx/sites-available/atv.christopherrutherford.net
sudo nginx -t
sudo systemctl reload nginx
```

Verify:

```sh
curl -I https://atv.christopherrutherford.net/healthz
curl -I https://atv.christopherrutherford.net/
```

## 3. Update Awesome TV

Set the public URL and keep the Cast base on the LAN address:

```sh
AWESOMETV_PUBLIC_URL=https://atv.christopherrutherford.net
AWESOMETV_CAST_BASE_URL=http://192.168.1.153:8409
```

Then recreate the app container:

```sh
docker compose up -d --force-recreate
```

## Notes

- HLS, Cast, catch-up, DVR, cloud proxy, and webhook paths have proxy buffering
  disabled and long read timeouts.
- The upstream is `127.0.0.1:8409`. If nginx runs in a container instead of on
  the host, change the upstream to `192.168.1.153:8409`.
- The current app still serves the protected admin console at `/`. Do not share
  the public hostname broadly until the public viewer/admin split is implemented.
- If Cloudflare or another proxy terminates TLS in front of nginx, set its SSL
  mode to **Full (strict)** and forward the original scheme/host headers.
- Certbot renewal is normally installed as a systemd timer by the certbot
  package. Verify with:

```sh
systemctl list-timers | grep certbot
```
