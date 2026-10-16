"""NEW binary on the OLD data: every legacy shape must keep working."""
import json, statistics, time
from lib import *

wait(f"{P}/System/Info/Public")
time.sleep(40)
old = json.load(open("/state/old.json"))

# A virtual token issued by the old binary: its backend tokens were plaintext, now encrypted.
ok, d = both_backends(old["alice"]["token"], old["alice"]["id"], "old-dev")
check("pre-upgrade alice token still works (encrypted backend tokens decrypt)", ok, d)

for user, pw, label in [("alice", "alicepw", "user-hash-keyed mappings"),
                        ("bob", "bobpass", "admin-key mapping"),
                        ("erin", "erinpass", "ambiguous plaintext mapping"),
                        ("frank", "frankpass", "raw-password-keyed mapping")]:
    st, _ = api_login(P, user, pw + "x")
    check(f"{user}: wrong password rejected", st == 401, f"status={st}")
    st, r = api_login(P, user, pw)
    check(f"{user}: login works ({label})", st == 200, f"status={st}")
    if st == 200:
        uid, tok = r["User"]["Id"], r["AccessToken"]
        st2, ms = movies(tok, uid)
        check(f"{user}: backend reachable after login", st2 == 200 and len(ms) > 0, f"status={st2} movies={len(ms)}")
        old[user].update(id=uid, token=tok)
st, a = api_login(P, "alice", "alicepw")
check("alice: second login (Argon2 verify) works", st == 200, f"status={st}")
if st != 200:
    raise SystemExit("cannot continue without alice")
check("alice: both backends after upgrade", *both_backends(a["AccessToken"], a["User"]["Id"]))

# carol was SSO-linked on the old binary (mappings re-keyed under the admin key).
kb = Browser()
st, url, page = sso_login(kb, "kc", "kclegacy", "kcpass")
tok, uid = token_from(page), user_id_from(page)
check("carol: pre-upgrade Keycloak link logs straight in", tok is not None, f"status={st} url={url}")
if tok:
    st2, ms = movies(tok, uid)
    check("carol: backend reachable via SSO token", st2 == 200 and len(ms) > 0, f"status={st2} movies={len(ms)}")

# Web UI login (credential lookup + verify; no SQL hash match any more).
for user, pw, want in [("alice", "alicepw", True), ("alice", "nope", False), ("ghost", "nope", False)]:
    b, st, url = ui_login(P, user, pw)
    check(f"web UI login {user}/{'right' if want else 'wrong'}", ("/login" not in url) == want, f"url={url}")

# Unknown user vs wrong password must cost the same (dummy Argon2 verify).
def t(user):
    s = time.perf_counter(); ui_login(P, user, "wrong-password"); return time.perf_counter() - s
for _ in range(2):
    t("alice"); t("ghost")
known = statistics.median(t("alice") for _ in range(9))
ghost = statistics.median(t("ghost") for _ in range(9))
check("timing: unknown user ~ wrong password", 0.5 < ghost / known < 2.0, f"known={known*1000:.0f}ms unknown={ghost*1000:.0f}ms")

# Password change in the web UI.
b, st, url = ui_login(P, "bob", "bobpass")
st, _, page = b.go("POST", f"{P}/ui/user/profile/password",
                   {"current_password": "bobpass", "new_password": "bobpass2", "confirm_password": "bobpass2"})
check("bob: password change accepted", "updated successfully" in page, page[:200].replace("\n", " "))
st, _ = api_login(P, "bob", "bobpass")
check("bob: old password rejected after change", st == 401, f"status={st}")
st, r = api_login(P, "bob", "bobpass2")
check("bob: new password works", st == 200, f"status={st}")
if st == 200:
    st2, ms = movies(r["AccessToken"], r["User"]["Id"])
    check("bob: backend reachable after password change", st2 == 200 and len(ms) > 0, f"status={st2}")

# QuickConnect authenticates from stored mappings without any password.
st, qc = req("POST", f"{P}/QuickConnect/Initiate", headers=hdr("qc-device"))
secret, code = qc.get("Secret") or qc.get("secret"), qc.get("Code") or qc.get("code")
st, ok = req("POST", f"{P}/QuickConnect/Authorize?code={code}", headers=hdr("e2e-dev", a["AccessToken"]))
check("quickconnect: alice authorizes code", st == 200 and ok in (True, b"true"), f"status={st} body={ok}")
st, r = req("POST", f"{P}/Users/AuthenticateWithQuickConnect", {"Secret": secret}, hdr("qc-device"))
check("quickconnect: device gets a token", st == 200, f"status={st}")
if st == 200:
    check("quickconnect: token reaches both backends", *both_backends(r["AccessToken"], r["User"]["Id"], "qc-device"))

json.dump(old, open("/state/new.json", "w"))
summary()
