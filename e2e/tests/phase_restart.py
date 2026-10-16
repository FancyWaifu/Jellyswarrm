"""After a restart of the new binary: the persisted key still opens everything."""
import json, time
from lib import *

wait(f"{P}/System/Info/Public")
time.sleep(40)
state = json.load(open("/state/sso.json"))
for who in ["alice", "erin", "frank", "kc_new", "dex_new", "ak_new", "aut_new"]:
    s = state.get(who)
    if s:
        st, ms = movies(s["token"], s["id"])
        check(f"restart: {who} token still works", st == 200 and len(ms) > 0, f"status={st} movies={len(ms)}")
for user, pw in [("alice", "alicepw"), ("bob", "bobpass2"), ("erin", "erinpass"), ("frank", "frankpass")]:
    st, r = api_login(P, user, pw)
    check(f"restart: {user} login", st == 200, f"status={st}")
    if st == 200:
        st2, ms = movies(r["AccessToken"], r["User"]["Id"])
        check(f"restart: {user} backend reachable", st2 == 200 and len(ms) > 0, f"status={st2}")
for slug, u, pw in [("kc", "kclinker", "kcpass"), ("dex", "linker@dex.test", "dexpass"), ("ak", "aklinker", "akpass"), ("aut", "autlinker", "autpass"), ("kc", "kclegacy", "kcpass")]:
    st, url, page = sso_login(Browser(), slug, u, pw)
    tok, uid = token_from(page), user_id_from(page)
    st2, ms = movies(tok, uid) if tok else (None, [])
    check(f"restart: {slug} SSO {u}", st2 == 200 and len(ms) > 0, f"status={st} movies={len(ms)}")
b, st, url = ui_login(P, "alice", "alicepw")
check("restart: web UI login", "/login" not in url, url)
summary()
