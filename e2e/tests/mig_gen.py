"""Fill an OLD build with realistic data through its own API (so rows are in that
version's own formats), then record what to expect after the upgrade.
usage: mig_gen.py <scenario> <users> <flags,comma-separated>
flags: odd (unusual names/passwords), ui (admin-created users), sso (Keycloak links),
       watch (mark items played), devices (2 devices per user)"""
import json, os, sys, time
from lib import *

scenario, n_users, flags = sys.argv[1], int(sys.argv[2]), set(filter(None, sys.argv[3].split(",")))
wait(f"{P}/System/Info/Public"); time.sleep(40)  # old builds: servers count as offline until the first health check

_n = [0]
def xff():
    """Older builds rate-limit per X-Forwarded-For: give each login its own address."""
    _n[0] += 1
    return {"X-Forwarded-For": f"172.31.{_n[0] // 250 % 250}.{_n[0] % 250 + 1}"}

def login(user, pw, dev):
    return req("POST", f"{P}/Users/AuthenticateByName", {"Username": user, "Pw": pw}, {**hdr(dev), **xff()})

ODD = ["Zoë Ångström", "o'brien", "李小龙", "user with spaces", "UPPER.lower-mixed_09", "émile+tag", "x" * 60]
ODD_PW = ["pässwörd ✓", "with 'quotes\" and \\backslash", "密码密码", "   leading and trailing   ",
          "p" * 200, "semi;colon&amp=equals", "emoji 🎬🍿"]
users = []
for i in range(n_users):
    odd = "odd" in flags and i < len(ODD)
    name = ODD[i] if odd else f"{scenario[:10]}-u{i:03d}"
    pw = ODD_PW[i] if odd else f"pw-{name}-{i}"
    servers = ["A", "B"] if i % 3 == 0 else (["A"] if i % 3 == 1 else ["B"])
    users.append({"name": name, "pw": pw, "servers": servers, "tokens": [], "kind": "api"})

for u in users:  # backend accounts (Jellyfin keeps them across scenarios: names are per scenario)
    try:
        for s in u["servers"]:
            jf_create_user(A if s == "A" else B, u["name"], u["pw"])
    except AssertionError as e:  # Jellyfin itself refuses some names
        u["backend_refused"] = str(e)[:80]
        print(f"  jellyfin refused user {u['name']!r}: {u['backend_refused']}")
users = [u for u in users if "backend_refused" not in u]

for i, u in enumerate(users):
    devices = ["dev-a", "dev-b"] if "devices" in flags and i % 2 == 0 else ["dev-a"]
    for d in devices:
        st, r = login(u["name"], u["pw"], f"{d}-{i}")
        if st == 200:
            u["tokens"].append({"token": r["AccessToken"], "dev": f"{d}-{i}"})
            u["id"] = r["User"]["Id"]
        else:
            u.setdefault("login_errors", []).append(st)
rejected = [u["name"] for u in users if not u["tokens"] and set(u.get("login_errors", [])) == {400}]
if rejected:  # the old build's own name/password validation: those accounts never existed there
    print(f"  info  old build refused {len(rejected)} unusual name(s)/password(s) (400): {rejected}")
ok_logins = sum(1 for u in users if u["tokens"])
check(f"{scenario}: old build logs users in", ok_logins + len(rejected) == len(users), f"{ok_logins}/{len(users)} errors={[u.get('login_errors') for u in users if not u['tokens']][:3]}")

# Browse (creates media mappings) and optionally mark things played.
for u in users[: max(3, len(users) // 3)]:
    if not u["tokens"]:
        continue
    t = u["tokens"][0]
    h = hdr(t["dev"], t["token"])
    st, items = req("GET", f"{P}/Items?Recursive=true&IncludeItemTypes=Movie&userId={u['id']}", headers=h)
    if st == 200 and "watch" in flags and items.get("Items"):
        iid = items["Items"][0]["Id"]
        st1, _ = req("POST", f"{P}/Users/{u['id']}/PlayedItems/{iid}", headers=h)
        u["played"] = items["Items"][0]["Name"] if st1 == 200 else None

# Admin-created users (mapping written by the admin UI, keyed differently in older builds).
if "ui" in flags:
    admin = Browser()
    admin.go("POST", f"{P}/ui/login", {"username": "admin", "password": "masterpw"}, headers=xff())
    for k in range(3):
        name, pw, bpw = f"{scenario[:10]}-ui{k}", f"uipw-{k}", f"uibk-{k}"
        jf_create_user(A, name, bpw)
        uid = ui_add_user(admin, name, pw, "http://jf-a:8096", name, bpw)
        st, r = login(name, pw, f"ui-dev-{k}")
        if uid and st == 200:
            users.append({"name": name, "pw": pw, "servers": ["A"], "tokens": [{"token": r["AccessToken"], "dev": f"ui-dev-{k}"}],
                          "id": r["User"]["Id"], "kind": "ui"})
    check(f"{scenario}: admin UI created users", sum(u["kind"] == "ui" for u in users) == 3, f"{sum(u['kind'] == 'ui' for u in users)}/3")

# SSO links to existing accounts (versions with OIDC only).
sso = []
if "sso" in flags and req("GET", f"{P}/sso/providers")[0] == 200:
    wait("http://keycloak:8080/realms/jsw/.well-known/openid-configuration", 300)
    for kc_user, target in (("kclinker", users[0]), ("kclegacy", users[3])):
        b = Browser()
        st, url, page = sso_login(b, "kc", kc_user, "kcpass")
        st, url, page = b.go("POST", f"{P}/sso/link", {"username": target["name"], "password": target["pw"]}, headers=xff())
        linked = token_from(page) is not None or "Signing" in page
        sso.append({"kc_user": kc_user, "target": target["name"], "linked": linked})
    check(f"{scenario}: SSO identities linked on the old build", all(s["linked"] for s in sso), str(sso))

os.makedirs("/state/mig", exist_ok=True)
json.dump({"users": users, "sso": sso}, open(f"/state/mig/{scenario}.json", "w"), indent=1)
print(f"{scenario}: generated {len(users)} users, {sum(len(u['tokens']) for u in users)} sessions, {len(sso)} SSO links")
summary()
