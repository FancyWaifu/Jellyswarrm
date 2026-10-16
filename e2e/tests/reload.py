"""Hot reload must never swap session_key; logins must keep working."""
import sys
from lib import *
wait(f"{P}/System/Info/Public")
admin, st, url = ui_login(P, "admin", "masterpw")
st, _, page = admin.go("POST", f"{P}/ui/settings/reload")
want_ok = sys.argv[1] == "ok"
check(f"reload {'accepted' if want_ok else 'refused'}", ("Configuration reloaded" in page) == want_ok, page[:120])
st, r = api_login(P, "alice", "alicepw")
check("alice login after reload attempt", st == 200, f"status={st}")
if st == 200:
    check("alice backends after reload attempt", *both_backends(r["AccessToken"], r["User"]["Id"]))
summary()
