#!/usr/bin/env python3
"""End-to-end SSO test for wave-2 IdPs, driven through Jellyswarrm's RP flow.

Run inside a container that SHARES jellyswarrm-proxy-dev's network namespace, so
localhost:3000 is Jellyswarrm and the IdP service names resolve:
  docker run --rm --network container:jellyswarrm-proxy-dev \
    -v $PWD/idp-lab:/t python:3.12-alpine \
    sh -c "pip install -q requests && python /t/test-wave2.py"
"""
import re, sys, urllib.parse, requests

JW = "http://localhost:3000"
POCKETID_KEY = "dev-lab-pocketid-static-api-key-0123456789abcdef"
POCKETID_USER_ID = "adad9bd2-74d5-42f3-9e86-247adcc59703"


def classify(resp):
    t = resp.text
    if "jellyfin_credentials" in t:
        return "complete"
    if "Create a new account" in t or "Link your account" in t:
        return "link"
    if "validation failed" in t or resp.status_code in (401, 400):
        return "fail"
    return "unknown"


def finish(name, s, cb_resp):
    """cb_resp = the /sso/callback response. Assert validation, then provision."""
    state = classify(cb_resp)
    if state == "fail":
        return False, f"callback rejected (HTTP {cb_resp.status_code}): {cb_resp.text[:160]}"
    if state == "complete":
        return True, "validated + already linked (login_complete)"
    if state == "link":
        # Provision a new account to prove the full path.
        prov = s.post(f"{JW}/sso/link", data={"create": "1"}, allow_redirects=True)
        if "jellyfin_credentials" in prov.text:
            return True, "validated id_token + provisioned new user (login_complete)"
        return True, f"validated id_token (link page shown); provision returned {prov.status_code}"
    return False, f"unexpected callback state: HTTP {cb_resp.status_code}: {cb_resp.text[:160]}"


# ---------------- Hydra: fully auto-accepting login/consent ----------------
def test_hydra():
    s = requests.Session()
    r = s.get(f"{JW}/sso/login/hydra", allow_redirects=False)
    auth = r.headers["Location"]
    # Follow hydra -> consent app -> hydra -> localhost:3000/sso/callback
    cb = s.get(auth, allow_redirects=True)
    return finish("hydra", s, cb)


# ---------------- Gitea: form login, then POST the OAuth grant ----------------
def test_gitea():
    s = requests.Session()
    lg = s.get("http://gitea:3000/user/login")
    m = re.search(r'name="_csrf"\s+value="([^"]+)"', lg.text)
    if not m:
        return False, "could not find gitea login _csrf"
    s.post("http://gitea:3000/user/login",
           data={"_csrf": m.group(1), "user_name": "testuser", "password": "Password1!"})
    cb = s.get(f"{JW}/sso/login/gitea", allow_redirects=True)
    if classify(cb) in ("link", "complete"):
        return finish("gitea", s, cb)
    # Landed on the Gitea "authorize application" grant page — POST it.
    fields = dict(re.findall(r'<input[^>]*name="([^"]+)"[^>]*value="([^"]*)"', cb.text))
    if "_csrf" not in fields:
        return False, f"no grant form on gitea authorize page: {cb.text[:140]}"
    fields["granted"] = "true"
    cb2 = s.post("http://gitea:3000/login/oauth/grant", data=fields, allow_redirects=True)
    return finish("gitea", s, cb2)


# ---------------- FusionAuth: hosted-login form POST, follow consent ----------------
def test_fusionauth():
    s = requests.Session()
    r = s.get(f"{JW}/sso/login/fusionauth", allow_redirects=False)
    auth = r.headers["Location"]
    q = dict(urllib.parse.parse_qsl(urllib.parse.urlsplit(auth).query))
    s.get(auth, allow_redirects=True)  # render login form + session cookie
    q["loginId"] = "testuser@example.com"
    q["password"] = "Password1!"
    # Following redirects walks consent -> /sso/callback -> Jellyswarrm.
    cb = s.post("http://fusionauth:9011/oauth2/authorize", data=q, allow_redirects=True)
    return finish("fusionauth", s, cb)


# ---------------- Pocket ID: one-time-token session + API authorize ----------------
def test_pocketid():
    s = requests.Session()
    r = s.get(f"{JW}/sso/login/pocketid", allow_redirects=False)
    auth = r.headers["Location"]
    q = dict(urllib.parse.parse_qsl(urllib.parse.urlsplit(auth).query))
    base = "http://pocketid.local:1411"
    # Mint a one-time access token for testuser and exchange it for a session.
    otat = requests.post(f"{base}/api/users/{POCKETID_USER_ID}/one-time-access-token",
                         headers={"X-API-KEY": POCKETID_KEY}, json={}).json()["token"]
    ex = requests.post(f"{base}/api/one-time-access-token/{otat}", allow_redirects=False)
    # The session cookie is Secure, so requests won't resend it over plain HTTP —
    # extract the token and pass it back as an explicit Cookie header.
    tok = ex.cookies.get("access_token")
    auth_hdr = {"Cookie": f"access_token={tok}"}
    body = {"clientID": q["client_id"], "scope": q.get("scope", "openid profile email"),
            "callbackURL": q["redirect_uri"], "nonce": q.get("nonce", ""),
            "state": q.get("state", "")}
    if "code_challenge" in q:
        body["codeChallenge"] = q["code_challenge"]
        body["codeChallengeMethod"] = q.get("code_challenge_method", "S256")
    ar = requests.post(f"{base}/api/oidc/authorize", json=body, headers=auth_hdr)
    j = {}
    try:
        j = ar.json()
    except Exception:
        pass
    code = j.get("code")
    if not code:
        return False, f"pocketid issued no code: {ar.status_code} {ar.text[:160]}"
    cb = s.get(f"{JW}/sso/callback",
               params={"code": code, "state": q.get("state", "")}, allow_redirects=True)
    return finish("pocketid", s, cb)


TESTS = [("Ory Hydra", test_hydra), ("Gitea", test_gitea),
         ("FusionAuth", test_fusionauth), ("Pocket ID", test_pocketid)]

if __name__ == "__main__":
    print("=" * 64)
    results = {}
    for name, fn in TESTS:
        try:
            ok, msg = fn()
        except Exception as e:
            ok, msg = False, f"exception: {type(e).__name__}: {e}"
        results[name] = ok
        print(f"{'PASS' if ok else 'FAIL'}  {name:12} — {msg}")
    print("=" * 64)
    print(f"{sum(results.values())}/{len(results)} passed")
    sys.exit(0 if all(results.values()) else 1)
