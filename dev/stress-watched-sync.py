#!/usr/bin/env python3
"""Stress/fuzz the cross-server watched-state sync. Run in a container sharing
jellyswarrm-proxy-dev's netns:
  docker run --rm --network container:jellyswarrm-proxy-dev -v $PWD:/t \
    python:3.12-alpine sh -c "pip install -q requests && python /t/stress-watched-sync.py"
"""
import sys, time, threading, requests

PROXY = "http://localhost:3000"
MOVIES2 = "http://jellyfin-movies2:8096"
IMDB = "tt0063350"
fails = []


def hdr(token=None, device="stress-proxy"):
    h = f'MediaBrowser Client="Stress", Device="d", DeviceId="{device}", Version="1"'
    if token:
        h += f', Token="{token}"'
    return {"Authorization": h, "Content-Type": "application/json"}


def proxy_login(user, pw, device):
    r = requests.post(f"{PROXY}/Users/AuthenticateByName", headers=hdr(None, device),
                      json={"Username": user, "Pw": pw})
    r.raise_for_status()
    s = requests.Session()
    tok, vuid = r.json()["AccessToken"], r.json()["User"]["Id"]
    s.get(f"{PROXY}/Users/{vuid}/Views", headers=hdr(tok, device))
    return s, tok, vuid


def proxy_movie_ids(s, vuid, tok, device):
    """Return {'[Movies]': id, '[Movies2]': id} for NOTLD as the proxy exposes it."""
    out = {}
    for _ in range(15):
        r = s.get(f"{PROXY}/Users/{vuid}/Items", params={
            "Recursive": "true", "IncludeItemTypes": "Movie", "Fields": "ProviderIds"},
            headers=hdr(tok, device))
        for it in r.json().get("Items", []):
            nm = it.get("Name", "")
            if "Living Dead" in nm and "[Movies]" in nm:
                out["[Movies]"] = it["Id"]
            if "Living Dead" in nm and "[Movies2]" in nm:
                out["[Movies2]"] = it["Id"]
        if len(out) == 2:
            return out
        time.sleep(1)
    return out


def backend_handle(base, user, pw, device):
    r = requests.post(f"{base}/Users/AuthenticateByName", headers=hdr(None, device),
                      json={"Username": user, "Pw": pw}).json()
    return r["AccessToken"], r["User"]["Id"]


def backend_notld(base, tok, uid, device):
    r = requests.get(f"{base}/Users/{uid}/Items", params={
        "Recursive": "true", "IncludeItemTypes": "Movie", "Fields": "UserData,ProviderIds"},
        headers=hdr(tok, device))
    for it in r.json().get("Items", []):
        if it.get("ProviderIds", {}).get("Imdb") == IMDB:
            return it
    return None


def backend_played(base, tok, uid, device):
    it = backend_notld(base, tok, uid, device)
    return it.get("UserData", {}).get("Played") if it else None


def backend_set(base, tok, uid, device, played):
    it = backend_notld(base, tok, uid, device)
    m = requests.post if played else requests.delete
    m(f"{base}/Users/{uid}/PlayedItems/{it['Id']}", headers=hdr(tok, device))


def wait_until(fn, want, tries=20, delay=0.7):
    for _ in range(tries):
        if fn() == want:
            return True
        time.sleep(delay)
    return False


def check(name, ok, detail=""):
    print(f"{'PASS' if ok else 'FAIL'}  {name}" + (f" — {detail}" if detail else ""))
    if not ok:
        fails.append(name)


# ============================================================================
def test_cross_user_isolation():
    # user1 = user/movies (Movies+Movies2); user2 = admin/password (all backends)
    s1, t1, v1 = proxy_login("user", "movies", "iso-user1")
    ids = proxy_movie_ids(s1, v1, t1, "iso-user1")
    assert "[Movies]" in ids and "[Movies2]" in ids, "missing NOTLD copies"

    u_tok, u_uid = backend_handle(MOVIES2, "user", "movies", "iso-chk-user")
    a_tok, a_uid = backend_handle(MOVIES2, "admin", "password", "iso-chk-admin")
    # clean both accounts on movies2
    backend_set(MOVIES2, u_tok, u_uid, "iso-chk-user", False)
    backend_set(MOVIES2, a_tok, a_uid, "iso-chk-admin", False)
    time.sleep(1)

    # user1 marks played via proxy
    s1.post(f"{PROXY}/Users/{v1}/PlayedItems/{ids['[Movies]']}", headers=hdr(t1, "iso-user1"))
    synced = wait_until(lambda: backend_played(MOVIES2, u_tok, u_uid, "iso-chk-user"), True)
    admin_state = backend_played(MOVIES2, a_tok, a_uid, "iso-chk-admin")
    check("cross-user isolation", synced and admin_state is not True,
          f"user-synced={synced}, admin-leaked={admin_state}")
    # cleanup
    s1.delete(f"{PROXY}/Users/{v1}/PlayedItems/{ids['[Movies]']}", headers=hdr(t1, "iso-user1"))


def test_toggle_race():
    s1, t1, v1 = proxy_login("user", "movies", "race-user")
    ids = proxy_movie_ids(s1, v1, t1, "race-user")
    mid = ids["[Movies]"]
    u_tok, u_uid = backend_handle(MOVIES2, "user", "movies", "race-chk")
    ok = True
    for ending in (True, False, True, False):
        # fire a rapid alternating burst ending on `ending` (no waits)
        seq = []
        state = not ending
        for _ in range(7):
            state = not state
            seq.append(state)
        seq[-1] = ending
        for st in seq:
            (s1.post if st else s1.delete)(
                f"{PROXY}/Users/{v1}/PlayedItems/{mid}", headers=hdr(t1, "race-user"))
        converged = wait_until(lambda: backend_played(MOVIES2, u_tok, u_uid, "race-chk"), ending)
        final = backend_played(MOVIES2, u_tok, u_uid, "race-chk")
        if not converged:
            ok = False
            print(f"   race: burst ending={ending} -> movies2={final} (NOT converged)")
    check("toggle-race convergence", ok, "4 rapid alternating bursts converge to last intent")
    s1.delete(f"{PROXY}/Users/{v1}/PlayedItems/{mid}", headers=hdr(t1, "race-user"))


def test_malformed_item_ids():
    s1, t1, v1 = proxy_login("user", "movies", "fuzz-user")
    payloads = [
        "00000000000000000000000000000000",   # well-formed but nonexistent
        "..%2F..%2Fetc%2Fpasswd",             # path traversal (encoded)
        "a'b\"c;drop",                         # injection-ish chars
        "x" * 4000,                            # very long
        "héllo-ünïcode-🎬",                     # unicode
        "",                                    # empty (routes elsewhere)
    ]
    crashed = False
    for p in payloads:
        try:
            r = s1.post(f"{PROXY}/Users/{v1}/PlayedItems/{p}", headers=hdr(t1, "fuzz-user"), timeout=20)
            if r.status_code >= 500:
                print(f"   malformed id -> HTTP {r.status_code} for {p[:40]!r}")
                crashed = True
        except requests.RequestException as e:
            print(f"   request error for {p[:40]!r}: {e}")
            crashed = True
    # proxy must still be alive and serving afterwards
    alive = requests.get(f"{PROXY}/System/Info/Public", timeout=10).status_code == 200
    check("malformed item ids handled", (not crashed) and alive,
          f"no 5xx/crash={not crashed}, proxy-alive={alive}")


def test_mixed_peers_no_match():
    # admin spans all 4 backends; Shows/Music have no matching movie -> no-op, no error
    s, t, v = proxy_login("admin", "password", "mixed-admin")
    ids = proxy_movie_ids(s, v, t, "mixed-admin")
    if "[Movies]" not in ids:
        check("mixed match/no-match peers", False, "admin couldn't see [Movies]"); return
    u_tok, u_uid = backend_handle(MOVIES2, "admin", "password", "mixed-chk")
    backend_set(MOVIES2, u_tok, u_uid, "mixed-chk", False); time.sleep(1)
    s.post(f"{PROXY}/Users/{v}/PlayedItems/{ids['[Movies]']}", headers=hdr(t, "mixed-admin"))
    synced = wait_until(lambda: backend_played(MOVIES2, u_tok, u_uid, "mixed-chk"), True)
    alive = requests.get(f"{PROXY}/System/Info/Public").status_code == 200
    check("mixed match/no-match peers", synced and alive,
          f"movies2-synced={synced}, proxy-alive={alive} (Shows/Music no-match, no error)")
    s.delete(f"{PROXY}/Users/{v}/PlayedItems/{ids['[Movies]']}", headers=hdr(t, "mixed-admin"))


if __name__ == "__main__":
    print("=" * 70)
    for fn in (test_cross_user_isolation, test_toggle_race,
               test_malformed_item_ids, test_mixed_peers_no_match):
        try:
            fn()
        except Exception as e:
            check(fn.__name__, False, f"exception: {type(e).__name__}: {e}")
    print("=" * 70)
    print(f"{'ALL PASS' if not fails else 'FAILURES: ' + ', '.join(fails)}")
    sys.exit(1 if fails else 0)
