"""Big merged library (A: Film 000-129, B: Film 100-229, 30 shared by imdb id):
paging, sort, Latest, Continue Watching across backends."""
import time, urllib.parse
from lib import *

def backend_wait(host, n):
    h = jf_admin(host)
    for _ in range(150):
        st, r = req("GET", f"{host}/Items?Recursive=true&IncludeItemTypes=Movie&Limit=0", headers=h)
        if st == 200 and r["TotalRecordCount"] >= n:
            return h
        time.sleep(2)
    raise SystemExit(f"{host} never indexed {n}")
ha, hb = backend_wait("http://jf-a:8096", 130), backend_wait("http://jf-b:8096", 130)

wait(f"{P}/System/Info/Public"); time.sleep(40)  # servers count as offline until the first health check
st, a = api_login(P, "admin", "password"); uid = a["User"]["Id"]; H = hdr(token=a["AccessToken"])
st, views = req("GET", f"{P}/Users/{uid}/Views", headers=H)
lib = next(v["Id"] for v in views["Items"] if v.get("CollectionType") == "movies")

def walk(sort, order, limit=100):
    names, totals, start = [], [], 0
    while True:
        q = f"ParentId={lib}&Recursive=true&IncludeItemTypes=Movie&SortBy={sort}&SortOrder={order}&StartIndex={start}&Limit={limit}&userId={uid}"
        st, r = req("GET", f"{P}/Items?{q}", headers=H)
        assert st == 200, (st, r)
        names += [i["Name"].split(" [")[0] for i in r["Items"]]; totals.append(r["TotalRecordCount"])
        if len(r["Items"]) > limit:
            return names, totals, f"page returned {len(r['Items'])} > Limit"
        start += limit
        if start >= r["TotalRecordCount"] or not r["Items"] or start > 1000:
            return names, totals, ""

names, totals, err = walk("SortName", "Ascending")
expect = [f"Film {i:03d}" for i in range(230)]
check("paging by name reaches all 230 unique titles", sorted(set(names)) == expect, f"unique={len(set(names))} {err}")
check("no title repeated across pages", len(names) == len(set(names)), f"rows={len(names)}")
check("pages come back in name order", names == sorted(names), f"first={names[:3]} around 100={names[98:102]}")
check("final TotalRecordCount is 230", totals[-1] == 230, f"totals per page={totals}")

names, totals, err = walk("ProductionYear,SortName", "Descending", limit=50)
check("descending year paging is globally ordered", names[:3] == ["Film 229", "Film 228", "Film 227"] and names == sorted(names, reverse=True) and len(set(names)) == 230,
      f"first={names[:3]} unique={len(set(names))} {err}")

st, latest = req("GET", f"{P}/Users/{uid}/Items/Latest?ParentId={lib}&Limit=16", headers=H)
check("Latest honours Limit=16", st == 200 and len(latest) <= 16, f"{st} n={len(latest) if st == 200 else latest}")

# Continue Watching: partly watch one A-only and one B-only film, B more recently.
def watch(host, h, title, when):
    st, r = req("GET", f"{host}/Items?Recursive=true&IncludeItemTypes=Movie&SearchTerm={urllib.parse.quote(title)}", headers=h)
    item = r["Items"][0]["Id"]
    st, me = req("GET", f"{host}/Users/Me", headers=h)
    st, _ = req("POST", f"{host}/UserItems/{item}/UserData?userId={me['Id']}",
                {"PlaybackPositionTicks": 3000000000, "LastPlayedDate": when, "Played": False}, h)
    assert st == 200, (host, st, _)
watch("http://jf-a:8096", ha, "Film 005", "2026-01-01T10:00:00.0000000Z")
watch("http://jf-b:8096", hb, "Film 205", "2026-02-01T10:00:00.0000000Z")
for path in ("/UserItems/Resume?Limit=12&MediaTypes=Video", f"/Users/{uid}/Items/Resume?Limit=12&MediaTypes=Video"):
    st, r = req("GET", f"{P}{path}{'&' if '?' in path else '?'}userId={uid}", headers=H)
    got = [i["Name"].split(" [")[0] for i in r.get("Items", [])] if st == 200 else r
    check(f"Continue Watching from both servers, newest first: {path.split('?')[0]}", got == ["Film 205", "Film 005"], f"{st} {got}")
summary()
