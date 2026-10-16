"""Wrong local password must not log in an existing user whose backend mapping
is admin-key encrypted (admin-created mapping)."""
import re, sys
from lib import *

P = "http://proxy:3000"
wait(f"{P}/System/Info/Public")
jf_create_user("http://jf-a:8096", "bobjf", "bobjfpass")
admin, st, url = ui_login(P, "admin", "masterpw")
assert "/login" not in url, ("admin ui login failed", st, url)
admin.go("POST", f"{P}/ui/users", {"username": "bob", "password": "bobpass"})
st, _, html = admin.go("GET", f"{P}/ui/users/list")
uid = re.search(r'name="user_id" value="([^"]+)"', html).group(1)
st, _, html = admin.go("POST", f"{P}/ui/users/mappings", {"user_id": uid, "server_url": "http://jf-a:8096",
                       "mapped_username": "bobjf", "mapped_password": "bobjfpass"})
print("mapping created:", "bobjf" in html)

ok = 0
st, _ = api_login(P, "bob", "totally-wrong")
print(("PASS" if st == 401 else "FAIL"), " wrong password rejected", f"status={st}"); ok += st == 401
st, _ = api_login(P, "bob", "bobpass")
print(("PASS" if st == 200 else "FAIL"), " right password still works", f"status={st}"); ok += st == 200
print(f"\n{ok}/2 passed")
