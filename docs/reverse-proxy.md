# Running behind a reverse proxy, HTTPS and SSO

Checklist for deployments where Jellyswarrm, the identity provider (IdP), or
both sit behind a reverse proxy, a tunnel, or Cloudflare.

## Jellyswarrm behind a proxy

* **Forward the original host and scheme.** The SSO callback URL is built from
  the request's `Host` and `X-Forwarded-Proto` headers. If the proxy doesn't
  pass `X-Forwarded-Proto: https`, the callback becomes `http://…/sso/callback`
  and the IdP rejects it as an unregistered redirect URI.
* **Optional: `public_url`** (e.g. `"https://jelly.example.com"`) fixes the
  callback URL instead of reading it from the headers. It is extra protection
  against a forged `Host`, which the IdP's exact redirect-URI check already
  rejects. If set, it must match the redirect URI registered with the IdP.
* **`secure_cookies = true`** when users reach Jellyswarrm over HTTPS.
* **`trusted_proxies`** — list the IP(s) of whatever connects to Jellyswarrm
  directly (nginx, Traefik, the cloudflared container). Without it, every user
  shares one login rate-limit bucket: a burst of 5 attempts, then one every
  10 seconds. Only the rightmost `X-Forwarded-For` hop (the one your proxy
  appended) is believed, and only from a listed peer.
* **Cloudflare tunnel:** also set `trust_cf_connecting_ip = true` so the real
  client IP is taken from `CF-Connecting-IP`. Enable it only if all traffic to
  the trusted proxy comes through Cloudflare; otherwise clients can forge it.

```toml
secure_cookies = true
trusted_proxies = ["172.18.0.5"]   # e.g. the cloudflared container
trust_cf_connecting_ip = true
```

## The identity provider

* **Register the redirect URI** `https://<your-jellyswarrm-host>/sso/callback`
  (plus your `url_prefix`, if any).
* **`issuer_url` must match the IdP's issuer exactly**, scheme and trailing
  slash included — e.g. Authentik `https://auth.example.com/application/o/<slug>/`,
  Keycloak `https://kc.example.com/realms/<realm>`. Jellyswarrm does not follow
  redirects during discovery (SSRF protection), so an `http://` URL that the
  IdP redirects to `https://` fails.
* **The IdP must know it is served over HTTPS.** Behind a proxy it builds its
  issuer from `X-Forwarded-Proto`; if that is missing it advertises `http://…`
  and validation fails with an issuer mismatch.
* **Server-to-server calls must not be challenged.** Jellyswarrm fetches the
  discovery document, JWKS, token and userinfo endpoints directly. Cloudflare
  Bot Fight Mode, "Under Attack" mode, managed challenges or Cloudflare Access
  in front of those paths make discovery fail. Add a WAF skip rule for them
  (Authentik: `/application/o/*`), or for Jellyswarrm's IP.
* **Private certificate authority:** outgoing HTTPS trusts the system store as
  well as the bundled public roots. Add your CA to the container's store, or set
  `SSL_CERT_FILE` to a bundle that contains it. Public certificates (Cloudflare,
  Let's Encrypt) need nothing extra.
* If the proxy log shows `provider discovery failed`, it is one of the points
  above: wrong issuer, a challenge page, a redirect, or an untrusted certificate.

Tested end to end with Keycloak 26, Dex 2.43, Authentik 2025.8 and Authelia 4.39
(over HTTPS with a private CA). Authelia keeps email and username out of the ID
token by default; Jellyswarrm then reads them from the userinfo endpoint.

## Back up the session key

`session_key` in `jellyswarrm.toml` encrypts every stored backend password and
session token. It is generated on first start and written to the config file.
**Back it up together with `jellyswarrm.db`** — a database without its key
cannot log anyone in to the backends. Jellyswarrm refuses to start on a config
file it can't parse rather than falling back to defaults, and a config reload
cannot change the key while running.
