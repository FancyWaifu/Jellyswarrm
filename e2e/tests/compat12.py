"""Web-12 calls through the proxy against a mixed 12.2 / 10.11 backend set."""
import time
from lib import *
wait(f"{P}/System/Info/Public"); time.sleep(40)
st, a = api_login(P, "admin", "password"); tok, uid = a["AccessToken"], a["User"]["Id"]; H = hdr(token=tok)
st, info = req("GET", f"{P}/System/Info", headers=H)
check("proxy reports the bundled web version", info.get("Version") == "12.2.0", f"Version={info.get('Version')}")
st, views = req("GET", f"{P}/Users/{uid}/Views", headers=H)
lib = next(v["Id"] for v in views["Items"] if v.get("CollectionType") == "movies")
st, items = req("GET", f"{P}/Items?ParentId={lib}&Recursive=true&IncludeItemTypes=Movie&userId={uid}", headers=H)
for i in items["Items"]:
    t = time.time()
    st, r = req("GET", f"{P}/Items/{i['Id']}/Collections?userId={uid}&fields=PrimaryImageAspectRatio", headers=H)
    print(f"  {i['Name']:24} {i.get('ProductionYear')} -> {st} {str(r)[:80]} ({time.time()-t:.2f}s)")
    check(f"collections call for {i['Name']} {i.get('ProductionYear')} is 200 or 404, fast", st in (200, 404) and time.time() - t < 5, f"status={st}")
    st, d = req("GET", f"{P}/Users/{uid}/Items/{i['Id']}", headers=H)
    check(f"item details for {i['Name']} {i.get('ProductionYear')}", st == 200, f"status={st}")
for ep in ("Filters", "Filters2"):
    for q, label in ((f"parentId={lib}&includeItemTypes=Movie", "merged library"), ("includeItemTypes=Movie", "no parent")):
        st, r = req("GET", f"{P}/Items/{ep}?userId={uid}&{q}", headers=H)
        check(f"/Items/{ep} ({label})", st == 200 and isinstance(r, dict) and "Genres" in r and (ep == "Filters2" or label == "no parent" or bool(r.get("Years"))), f"status={st} keys={list(r)[:5] if isinstance(r, dict) else r[:60]}")
summary()
