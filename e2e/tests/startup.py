"""Right after the proxy answers (no warm-up), results come from both servers.
usage: startup.py <label>"""
import sys, time
from lib import *
end = time.time() + 120
while time.time() < end:  # tight wait: first moment the proxy answers
    try:
        if req("GET", f"{P}/System/Info/Public", timeout=2)[0] == 200:
            break
    except Exception:
        pass
    time.sleep(0.2)
st, a = api_login(P, "admin", "password"); uid = a["User"]["Id"]; H = hdr(token=a["AccessToken"])
st, r = req("GET", f"{P}/Items?Recursive=true&IncludeItemTypes=Movie&userId={uid}", headers=H)
names = [i["Name"] for i in r["Items"]] if st == 200 else r
check(f"{sys.argv[1]}: first request sees both servers", st == 200 and any("[A]" in n for n in names) and any("[B]" in n for n in names), f"{st} {names}")
summary()
