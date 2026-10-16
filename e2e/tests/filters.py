"""Merged-library filter values and genres combine both backends
(A: Comedy/Drama, B: Comedy/Horror)."""
import time
from lib import *
wait(f"{P}/System/Info/Public"); time.sleep(40)  # servers count as offline until the first health check
server_id = req("GET", f"{P}/System/Info/Public")[1]["Id"]
st, a = api_login(P, "admin", "password"); uid = a["User"]["Id"]; H = hdr(token=a["AccessToken"])
st, views = req("GET", f"{P}/Users/{uid}/Views", headers=H)
lib = next(v["Id"] for v in views["Items"] if v.get("CollectionType") == "movies")
q = f"ParentId={lib}&IncludeItemTypes=Movie&userId={uid}"

st, f2 = req("GET", f"{P}/Items/Filters2?{q}", headers=H)
check("Filters2 genres from both servers", st == 200 and [g["Name"] for g in f2["Genres"]] == ["Comedy", "Drama", "Horror"], f"{st} {f2}")
st, f1 = req("GET", f"{P}/Items/Filters?{q}", headers=H)
check("Filters genres + years from both servers", st == 200 and f1["Genres"] == ["Comedy", "Drama", "Horror"] and f1["Years"] == list(range(1900, 2130)),
      f"{st} genres={f1.get('Genres') if st == 200 else f1} years={len(f1.get('Years', [])) if st == 200 else ''}")
st, g = req("GET", f"{P}/Genres?ParentId={lib}&userId={uid}&SortBy=SortName", headers=H)
check("Genres tab lists both servers' genres with the proxy's ServerId",
      st == 200 and [x["Name"] for x in g["Items"]] == ["Comedy", "Drama", "Horror"] and g["TotalRecordCount"] == 3 and all(x["ServerId"] == server_id for x in g["Items"]),
      f"{st} {[(x['Name'], x.get('ServerId', '')[:6]) for x in g['Items']] if st == 200 else g}")
st, gp = req("GET", f"{P}/Genres?ParentId={lib}&userId={uid}&SortBy=SortName&StartIndex=1&Limit=1", headers=H)
check("Genres paging applies to the combined list", st == 200 and [x["Name"] for x in gp["Items"]] == ["Drama"] and gp["TotalRecordCount"] == 3, f"{st} {gp}")

horror = next(x["Id"] for x in f2["Genres"] if x["Name"] == "Horror")
st, r = req("GET", f"{P}/Items?ParentId={lib}&Recursive=true&IncludeItemTypes=Movie&GenreIds={horror}&SortBy=SortName&Limit=100&userId={uid}", headers=H)
names = [i["Name"] for i in r["Items"]]
check("filtering by a B-only genre returns B's 65 films", st == 200 and r["TotalRecordCount"] == 65 and all("[B]" in n for n in names), f"{st} total={r.get('TotalRecordCount')} first={names[:2]}")

st, sb = req("GET", f"{P}/Items/Filters2?{q}", headers={**H, "X-Js-Server": "1"})
check("scoped to server A, filters stay A's own", st == 200 and [x["Name"] for x in sb["Genres"]] == ["Comedy", "Drama"], f"{st} {sb}")
st, g = req("GET", f"{P}/Genres?userId={uid}", headers=H)
check("Genres without a merged parent still answer", st == 200 and len(g["Items"]) >= 2, f"{st} {[x['Name'] for x in g['Items']] if st == 200 else g}")
summary()
