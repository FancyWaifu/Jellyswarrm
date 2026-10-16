"""Check users recorded by mig_gen.py against the running proxy.
usage: mig_verify.py <scenario> <phase> [skip=name1|name2] [down=B]
phase new      : first start of the new build: old tokens, passwords, wrong passwords, backends, UI, SSO
phase restart  : a sample again after a restart
phase rollback : old build on the restored backup: a sample can still log in
`skip` lists users expected to fail on purpose (corrupt/stale rows): they must fail cleanly.
`down` names a backend that is stopped: its content is not expected."""
import json, random, sys, time
from lib import *

scenario, phase = sys.argv[1], sys.argv[2]
opts = dict(a.split("=", 1) for a in sys.argv[3:])
skip = set(filter(None, opts.get("skip", "").split("|")))
down = opts.get("down")
data = json.load(open(f"/state/mig/{scenario}.json"))
users = [u for u in data["users"] if u.get("id")]
wait(f"{P}/System/Info/Public")
if phase == "rollback":
    time.sleep(40)  # old builds: first health check

n = [0]
def xff():
    """A distinct client IP per login: the runner is a trusted proxy in the upgraded
    config, so each login gets its own rate-limit bucket (old builds ignore it)."""
    n[0] += 1
    return {"X-Forwarded-For": f"10.{n[0] // 250 % 250}.{n[0] % 250}.{random.randint(1, 250)}"}

def login(u, pw, dev):
    if opts.get("pace"):  # no trusted proxy configured: stay inside one client's budget
        time.sleep(11)
    return req("POST", f"{P}/Users/AuthenticateByName", {"Username": u["name"], "Pw": pw}, {**hdr(dev), **xff()})

ITEM = {"A": ("Halloween", 1978), "B": ("Halloween", 2018)}  # one film only on each backend

def backends_ok(u, token, uid, dev):
    """Sees the A-only film iff on A, the B-only film iff on B (a stopped backend shows nothing)."""
    st, ms = movies(token, uid, dev)
    servers = u.get("servers_pre") or u["servers"]  # what the user had before the upgrade
    want = {ITEM[s] for s in servers if s != down}
    forbidden = {ITEM[s] for s in "AB" if s not in servers}
    seen = set(ms) & set(ITEM.values())
    return st == 200 and want <= seen and not (seen & forbidden), f"st={st} seen={sorted(seen)} want={sorted(want)}"

sample = users if phase == "new" else random.Random(1).sample(users, min(len(users), max(6, len(users) // 4)))
good = [u for u in sample if u["name"] not in skip]
bad = [u for u in sample if u["name"] in skip]
fails = {"token": [], "login": [], "backends": [], "wrongpw": []}
for i, u in enumerate(good):
    if phase == "new":
        for t in u["tokens"]:  # pre-upgrade sessions survive
            st, me = req("GET", f"{P}/Users/Me", headers=hdr(t["dev"], t["token"]))
            if st != 200:
                fails["token"].append((u["name"], st))
    st, r = login(u, u["pw"], f"verify-{phase}-{i}")
    if st != 200:
        fails["login"].append((u["name"], st))
        continue
    if phase != "rollback":
        ok, detail = backends_ok(u, r["AccessToken"], r["User"]["Id"], f"verify-{phase}-{i}")
        if not ok:
            fails["backends"].append((u["name"], detail))
    if i % 5 == 0:
        st, _ = login(u, u["pw"] + "x", f"verify-wrong-{i}")
        if st != 401:
            fails["wrongpw"].append((u["name"], st))
label = f"{scenario}/{phase}"
if phase == "new":
    check(f"{label}: pre-upgrade sessions still valid", not fails["token"], f"{sum(len(u['tokens']) for u in good)} sessions, failed={fails['token'][:4]}")
check(f"{label}: users log in with their passwords", not fails["login"], f"{len(good) - len(fails['login'])}/{len(good)} failed={fails['login'][:4]}")
if phase != "rollback":
    check(f"{label}: users see exactly their servers", not fails["backends"], f"failed={fails['backends'][:3]}")
check(f"{label}: wrong passwords rejected", not fails["wrongpw"], f"failed={fails['wrongpw'][:4]}")
if bad:
    codes = [login(u, u["pw"], f"verify-bad-{i}")[0] for i, u in enumerate(bad)]
    check(f"{label}: damaged accounts fail cleanly (no 5xx)", all(c < 500 for c in codes), f"codes={codes}")

if phase == "new":
    ui = [u for u in good if u["kind"] == "ui"]
    for u in ui:
        b = Browser()
        st, url, page = b.go("POST", f"{P}/ui/login", {"username": u["name"], "password": u["pw"]}, headers=xff())
        check(f"{label}: admin-created {u['name']} logs into the web UI", url.rstrip("/").endswith("/ui"), url)
    for s in data["sso"]:
        if not s["linked"]:
            continue
        b = Browser()
        st, url, page = sso_login(b, "kc", s["kc_user"], "kcpass")
        tok = token_from(page)
        check(f"{label}: SSO link {s['kc_user']} -> {s['target']} logs straight in", tok is not None, f"status={st} url={url[:80]}")

st, h = req("GET", f"{P}/System/Info/Public")
check(f"{label}: proxy healthy afterwards", st == 200, f"{st}")
summary()
