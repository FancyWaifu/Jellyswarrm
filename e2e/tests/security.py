"""Security regression suite. Every check states the safe behaviour: a FAIL is a finding.
Lines marked `jellyfin:` print behaviour Jellyfin itself has (reproduced against it
directly in the 2026-10 audit), so they're informational, not failures.
Users: alice (on A and B), mallory (only on A), admin (proxy admin, backend admin)."""
import http.client, json, re, time, urllib.parse
from lib import *

wait(f"{P}/System/Info/Public"); time.sleep(5)
for host in (A, B):
    jf_create_user(host, "alice", "alicepw")
jf_create_user(A, "mallory", "mallorypw")

st, a = api_login(P, "alice", "alicepw", "alice-dev"); atok, aid = a["AccessToken"], a["User"]["Id"]
st, m = api_login(P, "mallory", "mallorypw", "mal-dev"); mtok, mid = m["AccessToken"], m["User"]["Id"]
AH, MH = hdr("alice-dev", atok), hdr("mal-dev", mtok)
st, items = req("GET", f"{P}/Items?Recursive=true&IncludeItemTypes=Movie&userId={aid}", headers=AH)
alice_items = {i["Name"]: i["Id"] for i in items["Items"]}
b_item = next(v for k, v in alice_items.items() if "[B]" in k)
a_item = next(v for k, v in alice_items.items() if "[A]" in k and "Halloween" in k)
print("alice items:", list(alice_items))

def raw(method, path, headers=None, body=None):
    c = http.client.HTTPConnection("proxy", 3000, timeout=30)
    c.request(method, path, body=body, headers=headers or {})
    r = c.getresponse(); data = r.read()
    return r.status, dict(r.getheaders()), data

# ---------------------------------------------------------------- unauthenticated surface
print("\n== unauthenticated requests")
open_ok = {"/System/Info/Public", "/Users/Public", "/health", "/ready", "/Branding/Configuration", "/Branding/Css",
           "/QuickConnect/Enabled", "/sso/providers", "/web/index.html", "/", "/servers/inject.js", "/sso/inject.js"}
for path in ["/System/Info/Public", "/System/Info", "/Users/Public", "/Users", "/Sessions", "/Items",
             "/Library/VirtualFolders", "/System/Configuration", "/System/Logs", "/Plugins", "/ScheduledTasks",
             "/Environment/Drives", "/Startup/Configuration", "/Startup/User", "/Devices", "/Auth/Keys",
             "/servers", "/sso/providers", "/_debug/viewmerge", f"/Items/{a_item}", f"/Items/{a_item}/PlaybackInfo",
             f"/Videos/{a_item}/stream?static=true", f"/Videos/{b_item}/stream?static=true", f"/Items/{a_item}/Download",
             f"/Videos/{a_item}/master.m3u8", "/health", "/ready", "/Branding/Configuration", "/QuickConnect/Enabled"]:
    st, h, body = raw("GET", path)
    snippet = body[:90].decode(errors="replace").replace("\n", " ")
    if path in open_ok:
        print(f"  info  {path} -> {st} {snippet[:60]}")
    elif "/stream?" in path:
        print(f"  jellyfin: anonymous {path.split('?')[0][:20]}… -> {st} (Jellyfin serves streams anonymously; ids here are random)")
    else:
        check(f"no anonymous access: GET {path}", st in (401, 403, 404, 405) or (st == 302 and "login" in h.get("location", "")),
              f"{st} {h.get('content-type', '')} {snippet}")
st, h, body = raw("GET", "/Branding/Configuration")
check("anonymous branding shows no backend address", b"jf-a" not in body and b"jf-b" not in body and b"href" not in body, body[:160])

# ---------------------------------------------------------------- cross-user
print("\n== mallory (only on server A) vs alice")
st, r = req("GET", f"{P}/Items/{b_item}?userId={mid}", headers=MH)
check("mallory cannot read an item on a server she has no account on", st in (401, 403, 404), f"{st} {str(r)[:100]}")
st, h, body = raw("GET", f"/Videos/{b_item}/stream?static=true", MH)
check("mallory cannot stream from a server she has no account on", st in (401, 403, 404), f"{st} bytes={len(body)}")
req("POST", f"{P}/UserPlayedItems/{a_item}?userId={aid}", headers=AH)
st, r = req("GET", f"{P}/Users/{aid}/Items/{a_item}", headers=MH)
check("mallory naming alice's id gets her own data, not alice's", st in (401, 403, 404) or (r.get("UserData") or {}).get("Played") is False, f"{st} played={(r.get('UserData') or {}).get('Played') if isinstance(r, dict) else r}")
req("DELETE", f"{P}/UserPlayedItems/{a_item}?userId={aid}", headers=AH)
before = req("GET", f"{P}/Users/{aid}/Items/{a_item}", headers=AH)[1].get("UserData", {}).get("Played")
st, r = req("POST", f"{P}/Users/{aid}/PlayedItems/{a_item}", headers=MH)
st2, r2 = req("POST", f"{P}/UserPlayedItems/{a_item}?userId={aid}", headers=MH)
after = req("GET", f"{P}/Users/{aid}/Items/{a_item}", headers=AH)[1].get("UserData", {}).get("Played")
check("mallory cannot mark alice's items played", before == after, f"before={before} after={after} codes={st},{st2}")
st, r = req("POST", f"{P}/Users/{aid}/Password", {"CurrentPw": "", "NewPw": "pwned"}, MH)
st2, _ = api_login(P, "alice", "alicepw", "alice-dev2")
check("mallory cannot change alice's password", st2 == 200, f"change={st} alice_login_after={st2}")
st, r = req("GET", f"{P}/Users", headers=MH)
names = [u.get("Name") for u in r] if st == 200 and isinstance(r, list) else r
print("  jellyfin: non-admin lists all users:", st)
st, r = req("GET", f"{P}/Users/{aid}", headers=MH)
check("naming another user's profile returns the caller's own", st in (401, 403, 404) or (isinstance(r, dict) and r.get("Name") == "mallory"), f"{st} {str(r)[:80]}")
st, r = req("GET", f"{P}/Sessions", headers=MH)
others = [s.get("UserName") for s in r if s.get("UserName") not in ("mallory", None)] if st == 200 and isinstance(r, list) else []
check("non-admin /Sessions shows no other users' sessions", not others, f"{st} others={others}")
st, r = req("GET", f"{P}/System/Configuration", headers=MH)
print("  jellyfin: non-admin reads server configuration:", st)
st, r = req("GET", f"{P}/Library/VirtualFolders", headers=MH)
check("non-admin cannot list library folders/paths", st in (401, 403), f"{st} {str(r)[:120]}")
st, r = req("GET", f"{P}/Items/{a_item}?userId={mid}&Fields=Path,MediaSources", headers=MH)
path_leak = isinstance(r, dict) and (r.get("Path") or any(s.get("Path") for s in r.get("MediaSources") or []))
print("  jellyfin: non-admin sees file paths:", st)

# ---------------------------------------------------------------- token handling
print("\n== tokens")
st, pb = req("POST", f"{P}/Items/{a_item}/PlaybackInfo?userId={aid}", {"DeviceProfile": {"MaxStreamingBitrate": 1, "TranscodingProfiles": [{"Container": "ts", "Type": "Video", "VideoCodec": "h264", "AudioCodec": "aac", "Protocol": "hls"}]}}, AH)
blob = json.dumps(pb)
keys = set(re.findall(r'(?:api_key|ApiKey)=([0-9a-fA-F]{20,})', blob))
usable = [k for k in keys - {atok} if req("GET", f"{A}/Users/Me", headers={"X-Emby-Token": k})[0] == 200 or req("GET", f"{P}/Users/Me", headers={"X-Emby-Token": k})[0] == 200]
check("PlaybackInfo carries no other working credential", not usable, f"keys={[k[:6] for k in keys]} usable={[k[:6] for k in usable]}")
urls = re.findall(r'"(?:TranscodingUrl|Path|DirectStreamUrl)": "([^"]+)"', blob)
check("PlaybackInfo exposes no backend host", not any("jf-a" in u or "jf-b" in u or "8096" in u for u in urls), str(urls)[:200])
st, h, body = raw("GET", f"/Videos/{a_item}/master.m3u8?api_key={atok}&MediaSourceId={a_item}&VideoCodec=h264&AudioCodec=aac", )
pl = body.decode(errors="replace")
pk = set(re.findall(r'api_key=([0-9a-fA-F]{20,})', pl))
check("HLS playlist carries no backend token", pk <= {atok}, f"{st} keys={[k[:6] for k in pk]}")
st, r = req("POST", f"{P}/Sessions/Logout", headers=hdr("mal-dev", mtok))
st2, _ = req("GET", f"{P}/Users/Me", headers=MH)
check("logout revokes the proxy token", st2 == 401, f"logout={st} reuse={st2}")
st, m = api_login(P, "mallory", "mallorypw", "mal-dev"); mtok = m["AccessToken"]; MH = hdr("mal-dev", mtok)

st, pb2 = req("POST", f"{P}/Items/{a_item}/PlaybackInfo?userId={aid}", {"DeviceProfile": {"MaxStreamingBitrate": 1, "TranscodingProfiles": [{"Container": "ts", "Type": "Video", "VideoCodec": "h264", "AudioCodec": "aac", "Protocol": "hls"}]}}, AH)
turl = pb2["MediaSources"][0].get("TranscodingUrl", "")
st, h, body = raw("GET", re.sub(r"ApiKey=[0-9a-f]+&?", "", turl))
check("anonymous cannot fetch HLS for an item another user is playing", st in (401, 403, 404), f"{st} {body[:80]!r}")

# ---------------------------------------------------------------- quick connect
print("\n== quick connect")
st, qc = req("POST", f"{P}/QuickConnect/Initiate", headers=hdr("qc-dev"))
if st == 200:
    print(f"  code={qc.get('Code')} secret_len={len(qc.get('Secret', ''))}")
    check("Quick Connect secret is long and random", len(qc.get("Secret", "")) >= 32, f"len={len(qc.get('Secret', ''))}")
    st, r = req("GET", f"{P}/QuickConnect/Connect?secret=00", headers=hdr("qc-dev"))
    check("Quick Connect rejects a wrong secret", st in (400, 401, 403, 404), f"{st}")
    codes = [req("POST", f"{P}/QuickConnect/Authorize?code={n:06d}", headers=MH)[0] for n in range(12)]
    print("  jellyfin: Quick Connect code guesses not rate limited:", codes)
else:
    print(f"  initiate -> {st} {qc}")

# ---------------------------------------------------------------- management UI
print("\n== management UI")
pace(); c = http.client.HTTPConnection("proxy", 3000, timeout=30)
form = urllib.parse.urlencode({"username": "admin", "password": "masterpw", "next": "https://evil.example/phish"})
c.request("POST", "/ui/login", body=form, headers={"Content-Type": "application/x-www-form-urlencoded"})
r = c.getresponse(); loc = r.getheader("location") or ""; cookies = r.getheader("set-cookie") or ""
check("login does not redirect off-site (open redirect)", not loc.startswith(("http://", "https://", "//")), f"Location: {loc}")
check("session cookie is HttpOnly", "httponly" in cookies.lower(), cookies[:160])
check("session cookie is SameSite", "samesite" in cookies.lower(), cookies[:160])
form = urllib.parse.urlencode({"username": "admin", "password": "masterpw", "next": "//evil.example"})
pace(); c = http.client.HTTPConnection("proxy", 3000, timeout=30)
c.request("POST", "/ui/login", body=form, headers={"Content-Type": "application/x-www-form-urlencoded"})
r = c.getresponse(); loc2 = r.getheader("location") or ""
check("login does not redirect to a protocol-relative URL", not loc2.startswith("//"), f"Location: {loc2}")

pace(); ab, st, url = ui_login(P, "admin", "masterpw")
time.sleep(60)  # full limiter budget
codes = []
for i in range(8):
    c = http.client.HTTPConnection("proxy", 3000, timeout=10)
    c.request("POST", "/ui/login", body=urllib.parse.urlencode({"username": "admin", "password": f"wrong{i}"}),
              headers={"Content-Type": "application/x-www-form-urlencoded"})
    r = c.getresponse(); r.read(); codes.append(r.status)
check("UI login guessing is rate limited", 429 in codes and codes[:5].count(429) == 0, f"codes={codes}")
time.sleep(60)
st, url, html = ab.go("GET", f"{P}/ui/settings/form")
check("settings page does not echo the master password", "masterpw" not in html, f"{st} len={len(html)}")
check("settings page does not echo the session_key", "session_key" not in html.lower() or not re.search(r'[A-Za-z0-9+/]{80,}={0,2}', html), f"{st}")

pace(); ub, st, url = ui_login(P, "alice", "alicepw")
for path in ("/ui/users", "/ui/servers", "/ui/settings", "/ui/sso", "/ui/users/list", "/ui/settings/form"):
    st, url, html = ub.go("GET", f"{P}{path}")
    check(f"normal user is refused admin page {path}", st in (401, 403), f"{st} {url}")
st, url, html = ub.go("POST", f"{P}/ui/settings/save", {"password": "x"})
check("normal user cannot save settings", st in (401, 403), f"{st}")

# image proxy: a real item id still goes through (the test media have no art: 404)
orig = req("GET", f"{A}/Items?Recursive=true&IncludeItemTypes=Movie", headers=jf_admin(A))[1]["Items"][0]["Id"]
st, url, body = ub.go("GET", f"{P}/ui/user/media/image/1/{orig}")
check("UI image proxy still serves real item ids", st in (200, 404), f"{st}")
# image proxy path traversal (alice's own backend login)
for trav in ("..%2F..%2FSystem%2FInfo%3F", "..%2F..%2FUsers%3F", "..%2F..%2FSystem%2FConfiguration%3F"):
    st, url, body = ub.go("GET", f"{P}/ui/user/media/image/1/{trav}")
    check(f"UI image proxy cannot reach other backend endpoints: {urllib.parse.unquote(trav)}", not (st == 200 and body.lstrip().startswith(("{", "["))), f"{st} {body[:100]}")

# ---------------------------------------------------------------- headers / CORS
print("\n== headers")
st, h, body = raw("GET", "/System/Info/Public", {"Origin": "https://evil.example"})
print(f"  CORS on API for evil origin: ACAO={h.get('access-control-allow-origin')} ACAC={h.get('access-control-allow-credentials')}")
check("API CORS never allows credentials with a wildcard origin", h.get("access-control-allow-credentials", "").lower() != "true", "")
st, h, body = raw("GET", "/web/index.html")
print(f"  web client: x-frame-options={h.get('x-frame-options')} csp={str(h.get('content-security-policy'))[:60]}")
st, h, body = raw("GET", "/ui/login")
check("management UI forbids framing", h.get("x-frame-options", "").upper() == "DENY" or "frame-ancestors" in h.get("content-security-policy", ""), str(h.get("x-frame-options")))
st, h, body = raw("GET", "/System/Info/Public")
info = json.loads(body)
print(f"  public info: {dict((k, info.get(k)) for k in ('ServerName', 'Version', 'ProductName', 'LocalAddress', 'Id'))}")
check("public info does not leak a backend address", "jf-a" not in body.decode() and "jf-b" not in body.decode(), body[:150])

# ---------------------------------------------------------------- abuse
print("\n== abuse")
t = time.time()
st, r = req("GET", f"{P}/Items?Recursive=true&IncludeItemTypes=Movie&StartIndex=2000000000&Limit=100&userId={aid}", headers=AH, timeout=60)
print(f"  StartIndex=2e9: {st} in {time.time() - t:.1f}s")
check("huge StartIndex is answered quickly with an empty page", st == 200 and time.time() - t < 10 and not r["Items"], f"{st} {time.time() - t:.1f}s n={len(r.get('Items', [])) if isinstance(r, dict) else r}")
st, r = req("GET", f"{P}/Items?Recursive=true&StartIndex=18446744073709551615&Limit=18446744073709551615&userId={aid}", headers=AH, timeout=60)
check("overflowing StartIndex/Limit don't crash the proxy", st in (200, 400), f"{st}")
st, h, body = raw("GET", "/System/Info/Public")
check("proxy still up after abuse", st == 200, f"{st}")
summary()
