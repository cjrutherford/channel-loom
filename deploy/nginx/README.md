# nginx reverse proxy for atv.christopherrutherford.net

Exposes the Awesome TV app at:

```text
https://atv.christopherrutherford.net
```

There are two supported layouts:

| Layout | Config | Use when |
|---|---|---|
| SWAG container on the VPS | [`swag/atv.subdomain.conf`](swag/atv.subdomain.conf) | Current deployment: TLS terminates on `45.79.204.77`, app is reached over Tailscale |
| nginx on the app host | [`atv.christopherrutherford.net.conf`](atv.christopherrutherford.net.conf) | nginx and the app share a machine |

The `.http-only.conf` file is an ACME/bootstrap config for the on-host nginx
layout.

## Current deployment (SWAG)

```text
atv.christopherrutherford.net
  -> 45.79.204.77 (Linode VPS, SWAG container)
  -> nirvana.tail5a4a0.ts.net:8409 (Tailscale MagicDNS, home app host)
```

1. Copy the config into the SWAG proxy-confs directory:

   ```sh
   sudo cp deploy/nginx/swag/atv.subdomain.conf \
     /opt/swag/config/nginx/proxy-confs/atv.subdomain.conf
   ```

2. SWAG watches `proxy-confs` and reloads nginx. Confirm with:

   ```sh
   docker logs --tail 20 swag
   ```

3. Verify from outside the LAN:

   ```sh
   curl -I https://atv.christopherrutherford.net/healthz
   ```

   A `200` means DNS, TLS, the SWAG proxy, and the app are all working. The
   certificate is the shared SWAG certificate, which includes
   `atv.christopherrutherford.net` in its SAN list.

4. Set the app's public URL and keep Cast on the LAN address:

   ```sh
   AWESOMETV_BIND=0.0.0.0
   AWESOMETV_PUBLIC_URL=https://atv.christopherrutherford.net
   AWESOMETV_CAST_BASE_URL=http://192.168.1.153:8409
   ```

   Then recreate the container:

   ```sh
   docker compose up -d --force-recreate
   ```

### Troubleshooting

- **`ERR_CONNECTION_REFUSED` / `ERR_NETWORK_CHANGED` from one network but not
  others.** First rule out SWAG's fail2ban. The stock `nginx-unauthorized` jail
  bans an IP after five 401 responses, and a logged-out Awesome TV tab used to
  poll a protected API every ten seconds, so a real viewer could ban
  themselves. Check and clear it:

  ```sh
  docker exec swag fail2ban-client status
  docker exec swag fail2ban-client status nginx-unauthorized
  docker exec swag fail2ban-client set nginx-unauthorized unbanip <ip>
  ```

  The app now stops polling after a 401 and throttles its own login attempts, so
  disable the 401 jail to avoid recurrence. Edit
  `/opt/swag/config/fail2ban/jail.local`, set `enabled = false` under
  `[nginx-unauthorized]`, then sync and restart inside the container:

  ```sh
  docker exec swag cp /config/fail2ban/jail.local /etc/fail2ban/jail.local
  docker exec swag fail2ban-client restart
  ```

  The other SWAG jails (bad bots, bot search, deny, HTTP auth) can stay enabled.

- **Connections refused for a few seconds after a deploy.** SWAG and Docker
  rebuild their bridge/DNAT rules on restart. Confirm the port is open from an
  independent network, for example
  `https://check-host.net/check-tcp?host=45.79.204.77:443`.
- **HLS stalls or never starts over the public host.** The public stream is
  limited by the home upload, because every segment travels
  `app -> Tailscale -> VPS -> viewer`. Set `AWESOMETV_STREAM_MAX_KBPS` to a value
  the uplink can sustain (900 here) and keep the live buffer deep. Multiple
  public viewers share that same upload capacity.
- **502 from nginx.** The upstream is unreachable. Check inside the container:

  ```sh
  docker exec swag wget -qO- http://nirvana.tail5a4a0.ts.net:8409/healthz
  ```

  If MagicDNS does not resolve, use the Tailscale IP `100.120.253.100:8409`.
- **Login returns 401.** The session store is in memory, so recreating the app
  container signs everyone out. Log in again; the session cookie is now marked
  `Secure` when the request arrives over HTTPS.
- **Login returns 429.** Ten failed attempts from one client IP within 15
  minutes are throttled. Wait for the `Retry-After` interval.

## On-host nginx layout

Use this when nginx runs on the same machine as the app.

Prerequisites:

- DNS A/AAAA record for `atv.christopherrutherford.net` pointing at the public
  address.
- Router forwards TCP 80 and 443 to this host.
- nginx and certbot installed.
- Awesome TV listening on `0.0.0.0:8409`.

### 1. Bootstrap HTTP and obtain a certificate

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

### 2. Enable the TLS config

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

### 3. Update Awesome TV

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
- The on-host config assumes `127.0.0.1:8409`. If nginx is on a different
  machine, change the upstream to an address that machine can route to.
- The app still serves the protected admin console at `/`. Do not share the
  public hostname broadly until the public viewer/admin split is implemented.
- Certbot renewal is normally installed as a systemd timer by the certbot
  package. Verify with:

```sh
systemctl list-timers | grep certbot
```
