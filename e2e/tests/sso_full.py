"""SSO on the new binary against Keycloak, Dex and Authentik."""
import json, re, time
from lib import *

wait(f"{P}/System/Info/Public")
time.sleep(40)
state = json.load(open("/state/new.json"))
IDPS = [("kc", "kclinker", "kcnew", "kcpass"),
        ("dex", "linker@dex.test", "new@dex.test", "dexpass"),
        ("ak", "aklinker", "aknew", "akpass"),
        ("aut", "autlinker", "autnew", "autpass")]

for slug, linker, newbie, pw in IDPS:
    # Link an IdP identity to alice (Argon2 verifier now), with wrong tries first.
    b = Browser()
    st, url, page = sso_login(b, slug, linker, pw)
    check(f"{slug}: unknown identity gets link page", "/sso/link" in page, f"status={st}")
    for i in range(2):
        st, _, page = b.go("POST", f"{P}/sso/link", {"username": "alice", "password": "wrong"})
        check(f"{slug}: wrong link password {i+1} rejected", st == 401, f"status={st}")
    st, _, page = b.go("POST", f"{P}/sso/link", {"username": "alice", "password": "alicepw"})
    tok, uid = token_from(page), user_id_from(page)
    check(f"{slug}: link to alice with Argon2 password", tok is not None and uid == state["alice"]["id"], f"status={st} uid={uid}")
    if tok:
        check(f"{slug}: linked token reaches both backends", *both_backends(tok, uid))

    # Returning identity: straight in.
    st, url, page = sso_login(Browser(), slug, linker, pw)
    tok2 = token_from(page)
    check(f"{slug}: returning identity logs straight in", tok2 is not None, f"status={st}")

    # Three wrong passwords end the pending link.
    b = Browser()
    sso_login(b, slug, newbie, pw)
    codes = [b.go("POST", f"{P}/sso/link", {"username": "alice", "password": "wrong"})[0] for _ in range(3)]
    st, _, page = b.go("POST", f"{P}/sso/link", {"username": "alice", "password": "alicepw"})
    check(f"{slug}: link locked after 3 wrong passwords", codes == [401, 401, 401] and st == 400 and token_from(page) is None,
          f"codes={codes} then={st}")

    # Auto-provision a brand-new user (backend accounts created with admin creds).
    b = Browser()
    st, url, page = sso_login(b, slug, newbie, pw)
    st, _, page = b.go("POST", f"{P}/sso/link", {"create": "1"})
    tok, uid = token_from(page), user_id_from(page)
    check(f"{slug}: auto-provisioned new user", tok is not None, f"status={st} {page[:160]!r}")
    if tok:
        check(f"{slug}: provisioned user reaches both backends", *both_backends(tok, uid))
        st, me = req("GET", f"{P}/Users/{uid}", headers=hdr("e2e-dev", tok))
        name = me.get("Name", "") if isinstance(me, dict) else ""
        check(f"{slug}: provisioned user has a readable name", st == 200 and not re.fullmatch(r"[0-9a-f-]{32,36}", name), f"name={name!r}")
        state[f"{slug}_new"] = {"token": tok, "id": uid}

# QuickConnect for an SSO-only user (no known password at all).
sso = state.get("dex_new")
if sso:
    st, qc = req("POST", f"{P}/QuickConnect/Initiate", headers=hdr("qc-sso"))
    secret, code = qc.get("Secret") or qc.get("secret"), qc.get("Code") or qc.get("code")
    req("POST", f"{P}/QuickConnect/Authorize?code={code}", headers=hdr("e2e-dev", sso["token"]))
    st, r = req("POST", f"{P}/Users/AuthenticateWithQuickConnect", {"Secret": secret}, hdr("qc-sso"))
    check("quickconnect: SSO-provisioned user", st == 200, f"status={st}")
    if st == 200:
        check("quickconnect: SSO user token reaches both backends", *both_backends(r["AccessToken"], r["User"]["Id"], "qc-sso"))

json.dump(state, open("/state/sso.json", "w"))
summary()
