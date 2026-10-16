import json, time, urllib.request, urllib.error

AUTH = 'MediaBrowser Client="e2e", Device="e2e", DeviceId="{dev}", Version="1.0"{tok}'


def hdr(dev="e2e-dev", token=None):
    return {"Authorization": AUTH.format(dev=dev, tok=f', Token="{token}"' if token else "")}


def req(method, url, body=None, headers=None, timeout=30):
    data = json.dumps(body).encode() if body is not None else None
    h = {"Content-Type": "application/json", **(headers or {})}
    r = urllib.request.Request(url, data=data, headers=h, method=method)
    try:
        with urllib.request.urlopen(r, timeout=timeout) as resp:
            raw = resp.read()
            return resp.status, (json.loads(raw) if raw[:1] in (b"{", b"[") else raw)
    except urllib.error.HTTPError as e:
        return e.code, e.read()


def wait(url, secs=180):
    end = time.time() + secs
    while time.time() < end:
        try:
            if req("GET", url, timeout=5)[0] < 500:
                return
        except Exception:
            pass
        time.sleep(2)
    raise SystemExit(f"timeout waiting for {url}")


import http.cookiejar, urllib.parse


class Browser:
    """Cookie-keeping form client (a stand-in for a real browser session)."""

    def __init__(self):
        self.jar = http.cookiejar.CookieJar()
        self.op = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(self.jar))

    def go(self, method, url, form=None, headers=None):
        if url.endswith("/sso/link"):
            pace()
        data = urllib.parse.urlencode(form).encode() if form is not None else None
        r = urllib.request.Request(url, data=data, method=method, headers=headers or {})
        try:
            with self.op.open(r, timeout=60) as resp:
                return resp.status, resp.geturl(), resp.read().decode(errors="replace")
        except urllib.error.HTTPError as e:
            return e.code, e.geturl(), e.read().decode(errors="replace")


def jf_admin(host):
    st, a = req("POST", f"{host}/Users/AuthenticateByName", {"Username": "admin", "Pw": "password"}, hdr("setup"))
    return hdr("setup", a["AccessToken"])


def jf_create_user(host, name, pw):
    """Create a Jellyfin user (idempotent) and return its id."""
    h = jf_admin(host)
    st, users = req("GET", f"{host}/Users", headers=h)
    for u in users:
        if u["Name"] == name:
            return u["Id"]
    st, u = req("POST", f"{host}/Users/New", {"Name": name, "Password": pw}, h)
    assert st == 200, (host, name, st, u)
    return u["Id"]


def ui_login(proxy, user, pw):
    b = Browser()
    st, url, body = b.go("POST", f"{proxy}/ui/login", {"username": user, "password": pw})
    return b, st, url


_bucket = {"tokens": 5.0, "t": time.time()}


def pace():
    """Mirror the proxy's per-IP auth limiter (burst 5, +1 every 10 s) so tests
    never trip it unintentionally; the limiter itself is tested separately."""
    now = time.time()
    _bucket["tokens"] = min(5.0, _bucket["tokens"] + (now - _bucket["t"]) / 10.5)
    _bucket["t"] = now
    if _bucket["tokens"] < 1:
        time.sleep((1 - _bucket["tokens"]) * 10.5)
        _bucket["tokens"], _bucket["t"] = 1.0, time.time()
    _bucket["tokens"] -= 1


def api_login(proxy, user, pw, dev="e2e-dev"):
    pace()
    return req("POST", f"{proxy}/Users/AuthenticateByName", {"Username": user, "Pw": pw}, hdr(dev))


import html as _html, re as _re


def form_action(page, url):
    """(absolute action URL, hidden inputs) of the first <form> in page."""
    m = _re.search(r'<form[^>]*action="([^"]*)"', page)
    action = urllib.parse.urljoin(url, _html.unescape(m.group(1)) if m else "")
    hidden = dict(_re.findall(r'<input[^>]*type="hidden"[^>]*name="([^"]+)"[^>]*value="([^"]*)"', page))
    return action, hidden


def jsonreq(b, method, url, body=None, headers=None):
    """JSON request through a Browser (keeps its cookies)."""
    data = json.dumps(body).encode() if body is not None else None
    h = {"Content-Type": "application/json", "Accept": "application/json", **(headers or {})}
    r = urllib.request.Request(url, data=data, method=method, headers=h)
    try:
        with b.op.open(r, timeout=60) as resp:
            raw = resp.read()
            return resp.status, resp.geturl(), (json.loads(raw) if raw[:1] in (b"{", b"[") else raw.decode(errors="replace"))
    except urllib.error.HTTPError as e:
        return e.code, e.geturl(), e.read().decode(errors="replace")


def sso_login(b, slug, user, pw, proxy="http://proxy:3000"):
    """Drive /sso/login/<slug> through the IdP's login UI; returns the proxy's
    (status, url, html) after the callback (link page or completion page)."""
    st, url, page = b.go("GET", f"{proxy}/sso/login/{slug}")
    if slug == "kc":
        action, hidden = form_action(page, url)
        return b.go("POST", action, {**hidden, "username": user, "password": pw, "credentialId": ""})
    if slug == "dex":
        action, hidden = form_action(page, url)
        return b.go("POST", action, {**hidden, "login": user, "password": pw})
    if slug == "ak":
        # Authentik's login UI is an SPA over the flow-executor API.
        parsed = urllib.parse.urlparse(url)
        q = urllib.parse.parse_qs(parsed.query)
        flow = parsed.path.rstrip("/").split("/")[-1]
        ex = f"{parsed.scheme}://{parsed.netloc}/api/v3/flows/executor/{flow}/?" + urllib.parse.urlencode(
            {"query": urllib.parse.urlencode({"next": q.get("next", [""])[0]})})
        csrf = lambda: {"X-authentik-CSRF": next((c.value for c in b.jar if c.name == "authentik_csrf"), "")}
        st, _, ch = jsonreq(b, "GET", ex, headers=csrf())
        for _ in range(6):
            comp = ch.get("component") if isinstance(ch, dict) else None
            if comp == "ak-stage-identification":
                body = {"component": comp, "uid_field": user}
                if ch.get("password_fields"):
                    body["password"] = pw
            elif comp == "ak-stage-password":
                body = {"component": comp, "password": pw}
            elif comp == "xak-flow-redirect":
                return b.go("GET", urllib.parse.urljoin(ex, ch["to"]))
            else:
                raise SystemExit(f"authentik: unexpected challenge {ch!r:.300}")
            st, _, ch = jsonreq(b, "POST", ex, body, csrf())
        raise SystemExit("authentik: flow did not finish")
    if slug == "aut":
        # Authelia's portal is an SPA; first factor goes through its JSON API.
        parsed = urllib.parse.urlparse(url)
        q = {k: v[0] for k, v in urllib.parse.parse_qs(parsed.query).items()}
        base = f"{parsed.scheme}://{parsed.netloc}"
        body = {"username": user, "password": pw, "keepMeLoggedIn": False}
        if "flow_id" in q:
            body.update(flowID=q["flow_id"], flow=q.get("flow"), subflow=q.get("subflow"))
        elif "rd" in q:
            body.update(targetURL=q["rd"], requestMethod=q.get("rm", "GET"))
        st, _, r = jsonreq(b, "POST", f"{base}/api/firstfactor", body)
        if st != 200 or not isinstance(r, dict) or r.get("status") != "OK":
            raise SystemExit(f"authelia firstfactor: {st} {r!r:.300} (portal url {url})")
        nxt = (r.get("data") or {}).get("redirect") or q.get("rd")
        if not nxt:
            raise SystemExit(f"authelia: no redirect after first factor ({r!r:.200}, portal url {url})")
        return b.go("GET", urllib.parse.urljoin(base, nxt))
    raise ValueError(slug)


def token_from(page):
    m = _re.search(r'"AccessToken"\s*:\s*"([^"]+)"', page or "")
    return m.group(1) if m else None


def user_id_from(page):
    m = _re.search(r'"UserId"\s*:\s*"([^"]+)"', page or "")
    return m.group(1) if m else None


P = "http://proxy:3000"
A = "http://jf-a:8096"
B = "http://jf-b:8096"
HEX40 = "0123456789abcdef0123456789abcdef01234567"
RESULTS = []


def check(name, ok, detail=""):
    RESULTS.append(bool(ok))
    print(f"{'PASS' if ok else 'FAIL'}  {name}  {detail}", flush=True)
    return ok


def summary():
    print(f"\n{sum(RESULTS)}/{len(RESULTS)} passed", flush=True)


def ui_user_id(admin, username):
    st, _, page = admin.go("GET", f"{P}/ui/users/list")
    m = _re.search(rf'<article id="user-([^"]+)" data-username="{_re.escape(username)}"', page)
    return m.group(1) if m else None


def ui_add_user(admin, username, pw, server_url=None, mapped_user=None, mapped_pw=None):
    admin.go("POST", f"{P}/ui/users", {"username": username, "password": pw})
    uid = ui_user_id(admin, username)
    if server_url:
        admin.go("POST", f"{P}/ui/users/mappings", {"user_id": uid, "server_url": server_url,
                                                     "mapped_username": mapped_user, "mapped_password": mapped_pw})
    return uid


def movies(token, uid, dev="e2e-dev"):
    """(name, year) of every movie the proxy shows this user (merged library)."""
    h = hdr(dev, token)
    st, views = req("GET", f"{P}/Users/{uid}/Views", headers=h)
    if st != 200:
        return st, []
    out = []
    for v in views["Items"]:
        if v.get("CollectionType") == "movies":
            st, items = req("GET", f"{P}/Items?ParentId={v['Id']}&Recursive=true&IncludeItemTypes=Movie&userId={uid}", headers=h)
            out += [(_re.sub(r" \[[^\]]+\]$", "", i["Name"]), i.get("ProductionYear")) for i in items.get("Items", [])] if st == 200 else []
    return 200, sorted(out, key=str)


def both_backends(token, uid, dev="e2e-dev"):
    """True when movies only on A (1978) and only on B (2018) are both visible."""
    st, ms = movies(token, uid, dev)
    return ("Halloween", 1978) in ms and ("Halloween", 2018) in ms, f"status={st} movies={ms}"
