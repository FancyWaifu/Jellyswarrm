"""/servers needs a session; SSO redirect_uri ignores Host when public_url is set.
usage: hardening.py <expected redirect_uri prefix>"""
import http.client, sys, time, urllib.parse
from lib import *

wait(f"{P}/System/Info/Public")
st, body = req("GET", f"{P}/servers")
check("/servers without token -> 401", st == 401, f"status={st}")
st, body = req("GET", f"{P}/servers", headers={"X-Emby-Token": "bogus"})
check("/servers with bogus token -> 401", st == 401, f"status={st}")
st, a = api_login(P, "admin", "password")
tok = a["AccessToken"]
st, body = req("GET", f"{P}/servers", headers={"X-Emby-Token": tok})
check("/servers with X-Emby-Token -> names", st == 200 and sorted(s["name"] for s in body) == ["A", "B"], f"{st} {body}")
st, body = req("GET", f"{P}/servers", headers=hdr(token=tok))
check("/servers with MediaBrowser auth header -> 200", st == 200, f"status={st}")

wait("http://keycloak:8080/realms/jsw/.well-known/openid-configuration", 300)
c = http.client.HTTPConnection("proxy", 3000, timeout=30)
c.request("GET", "/sso/login/kc", headers={"Host": "evil.example", "X-Forwarded-Proto": "https"})
r = c.getresponse(); loc = r.getheader("location") or ""
ru = urllib.parse.parse_qs(urllib.parse.urlparse(loc).query).get("redirect_uri", [""])[0]
check(f"spoofed Host -> redirect_uri {sys.argv[1]}", ru == sys.argv[1], f"status={r.status} redirect_uri={ru}")
summary()
