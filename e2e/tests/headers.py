"""Client-only headers never reach a backend (B sits behind tests/header_spy.py):
the admin UI's session cookie, Referer/Origin, forwarding addresses, Cloudflare and
Jellyswarrm-internal headers; backends see Jellyswarrm's own user agent. Browsing,
playback info and streaming keep working."""
import json, time, urllib.request
from lib import *

wait(f"{P}/System/Info/Public"); time.sleep(25)
admin = Browser()
admin.go("POST", f"{P}/ui/login", {"username": "admin", "password": "masterpw"})
cookie = "; ".join(f"{c.name}={c.value}" for c in admin.jar)
check("setup: signed into the admin UI (session cookie)", bool(cookie))
jf_create_user(B, "hdruser", "hdr-pw-1")
st, a = req("POST", f"{P}/Users/AuthenticateByName", {"Username": "hdruser", "Pw": "hdr-pw-1"}, hdr("hdr-dev"))
check("setup: user logs in through the proxy", st == 200, f"{st}")
tok, uid = a["AccessToken"], a["User"]["Id"]
mark = sum(1 for _ in open("/state/spy.jsonl"))
CLIENT = {"Cookie": cookie, "Referer": "https://swarm.example/web/index.html", "Origin": "https://swarm.example",
          "User-Agent": "Python-urllib/3.12", "X-Forwarded-For": "203.0.113.9", "X-Forwarded-Proto": "https",
          "CF-Connecting-IP": "203.0.113.9", "CF-Ray": "8d1e2f-LAX", "Forwarded": "for=203.0.113.9"}
h = {**hdr("hdr-dev", tok), **CLIENT}
st, views = req("GET", f"{P}/UserViews?userId={uid}", headers=h)
check("library list works", st == 200 and views.get("Items"), f"{st}")
st, items = req("GET", f"{P}/Items?userId={uid}&Recursive=true&IncludeItemTypes=Movie", headers=h)
film = next((i for i in items.get("Items", []) if i.get("ProductionYear") == 2018), None)
check("merged movie list works and includes B's film", st == 200 and film is not None, f"{st}")
st, _ = req("GET", f"{P}/Users/{uid}/Items/{film['Id']}", headers=h)
check("item details work", st == 200, f"{st}")
st, pi = req("POST", f"{P}/Items/{film['Id']}/PlaybackInfo?UserId={uid}", {}, h)
check("playback info works", st == 200 and pi.get("MediaSources"), f"{st}")
r = urllib.request.urlopen(urllib.request.Request(f"{P}/Videos/{film['Id']}/stream?static=true", headers={**h, "Range": "bytes=0-65535"}), timeout=60)
check("streaming a range works", r.status in (200, 206) and len(r.read()) > 0, f"{r.status}")
st, _ = req("GET", f"{P}/Users/{uid}/Items/Latest?Limit=8", headers=h)
check("latest items work", st == 200, f"{st}")
st, _ = req("GET", f"{P}/Items/{film['Id']}/Images/Primary", headers=h)
check("image requests are answered (no 5xx)", st < 500, f"{st}")

seen = [json.loads(l) for i, l in enumerate(open("/state/spy.jsonl")) if i >= mark]
mine = [r for r in seen if "/system/info/public" not in r["p"].lower()]
check("setup: the test's requests reached B", len(mine) >= 6, f"{len(mine)} requests")
leaked = sorted({k for r in mine for k in r["h"] if k in ("cookie", "referer", "origin", "forwarded", "x-forwarded-for",
                 "x-forwarded-proto", "cf-connecting-ip", "cf-ray", "x-js-server")})
check("no client-only header reached the backend (cookie, referer, origin, forwarding, cf-*)", not leaked, str(leaked))
check("the admin session cookie never reached the backend", not any(cookie.split("=", 1)[1] in json.dumps(r["h"]) for r in seen))
uas = sorted({r["h"].get("user-agent", "") for r in mine})
check("backends see Jellyswarrm's user agent, not the client's", uas and all(u.startswith("Jellyswarrm/") for u in uas), str(uas))
summary()
