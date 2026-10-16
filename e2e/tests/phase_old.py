"""OLD binary: create every kind of legacy credential state, save handles in /state."""
import json, time
from lib import *

wait(f"{P}/System/Info/Public")
time.sleep(40)  # backends are marked healthy after the first health check
for host in (A, B):
    jf_create_user(host, "alice", "alicepw")
jf_create_user(A, "carol", "carolpw")
jf_create_user(A, "bobjf", "bobjfpass")
jf_create_user(B, "erinjf", HEX40)
jf_create_user(B, "frankjf", "frankjfpw")

state = {}
# alice: first API login -> local SHA user, mappings keyed by her hash, plaintext tokens.
st, a = api_login(P, "alice", "alicepw", "old-dev")
check("old: alice first login", st == 200, f"status={st}")
state["alice"] = {"token": a["AccessToken"], "id": a["User"]["Id"]}
check("old: alice sees both backends", *both_backends(a["AccessToken"], a["User"]["Id"], "old-dev"))

# carol: API login, then link a Keycloak identity (old code re-keys her mappings under the admin key).
st, c = api_login(P, "carol", "carolpw")
check("old: carol first login", st == 200, f"status={st}")
kb = Browser()
st, url, page = sso_login(kb, "kc", "kclegacy", "kcpass")
st, url, page = kb.go("POST", f"{P}/sso/link", {"username": "carol", "password": "carolpw"})
check("old: kclegacy linked to carol", token_from(page) is not None, f"status={st}")

# Admin-UI users: bob (admin-key mapping); erin/frank get rewritten to older formats on the host.
admin, st, url = ui_login(P, "admin", "masterpw")
for u, pw, srv, ju, jp in [("bob", "bobpass", A + "/", "bobjf", "bobjfpass"),
                           ("erin", "erinpass", B, "erinjf", HEX40),
                           ("frank", "frankpass", B + "/", "frankjf", "frankjfpw")]:
    state[u] = {"id": ui_add_user(admin, u, pw, srv, ju, jp)}
    check(f"old: admin created {u} with mapping", state[u]["id"] is not None)
st, b = api_login(P, "bob", "bobpass")
check("old: bob login", st == 200, f"status={st}")

json.dump(state, open("/state/old.json", "w"))
summary()
