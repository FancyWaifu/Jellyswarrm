#!/usr/bin/env python3
"""E2E: cross-server watched-state sync, driven through the Jellyswarrm proxy.

Marks "Night of the Living Dead" played on the Movies backend via the proxy and
verifies the *same movie* (matched by ProviderIds) becomes Played on the Movies2
backend — checked both through the proxy and directly on the movies2 backend.

Run in a container sharing jellyswarrm-proxy-dev's netns:
  docker run --rm --network container:jellyswarrm-proxy-dev \
    -v $PWD:/t python:3.12-alpine sh -c "pip install -q requests && python /t/test-watched-sync.py"
"""
import sys, time, requests

PROXY = "http://localhost:3000"
MOVIES2 = "http://jellyfin-movies2:8096"
DEVID = "watched-sync-test-device"        # the proxy client device
DEVID2 = "ground-truth-checker-device"    # distinct: must NOT collide with the
USER, PW = "user", "movies"               # proxy's backend session (one per device)
IMDB = "tt0063350"  # Night of the Living Dead — on Movies and Movies2


def hdr(token=None, device=DEVID):
    h = f'MediaBrowser Client="WatchTest", Device="pytest", DeviceId="{device}", Version="1"'
    if token:
        h += f', Token="{token}"'
    return {"Authorization": h, "Content-Type": "application/json"}


def proxy_played(s, vuid, tok, name_tag):
    """Played state of the NOTLD copy tagged [name_tag], as the proxy reports it.
    Lists all movies and filters client-side (proxy SearchTerm is unreliable)."""
    r = s.get(f"{PROXY}/Users/{vuid}/Items",
              params={"Recursive": "true", "IncludeItemTypes": "Movie",
                      "Fields": "UserData,ProviderIds"}, headers=hdr(tok))
    for it in r.json().get("Items", []):
        if name_tag in it.get("Name", "") and "Living Dead" in it.get("Name", ""):
            return it, it.get("UserData", {}).get("Played")
    return None, None


def movies2_notld(m2tok, m2uid):
    """The NOTLD movie on the movies2 backend (id + UserData), or None.
    Matches by ProviderIds client-side (server AnyProviderIdEquals is ignored)."""
    r = requests.get(f"{MOVIES2}/Users/{m2uid}/Items",
                     params={"Recursive": "true", "IncludeItemTypes": "Movie",
                             "Fields": "UserData,ProviderIds"}, headers=hdr(m2tok, DEVID2))
    for it in r.json().get("Items", []):
        if it.get("ProviderIds", {}).get("Imdb") == IMDB:
            return it
    return None


def movies2_played(m2tok, m2uid):
    """Ground truth: Played state of NOTLD on the movies2 backend directly."""
    it = movies2_notld(m2tok, m2uid)
    return it.get("UserData", {}).get("Played") if it else None


def wait_until(fn, want, tries=15, delay=1.0):
    for _ in range(tries):
        if fn() == want:
            return True
        time.sleep(delay)
    return False


def main():
    s = requests.Session()
    # Auth through the proxy (federates user across Movies + Movies2).
    r = s.post(f"{PROXY}/Users/AuthenticateByName", headers=hdr(),
               json={"Username": USER, "Pw": PW})
    assert r.status_code == 200, f"proxy auth {r.status_code}: {r.text[:200]}"
    tok, vuid = r.json()["AccessToken"], r.json()["User"]["Id"]
    s.get(f"{PROXY}/Users/{vuid}/Views", headers=hdr(tok))  # establish backend sessions
    time.sleep(2)

    # Direct movies2 handle (ground-truth check).
    m2 = requests.post(f"{MOVIES2}/Users/AuthenticateByName", headers=hdr(None, DEVID2),
                       json={"Username": USER, "Pw": PW}).json()
    m2tok, m2uid = m2["AccessToken"], m2["User"]["Id"]

    # Federation may take a moment to surface both backends' copies.
    src_item = peer_item = None
    for _ in range(15):
        src_item, _ = proxy_played(s, vuid, tok, "[Movies]")
        peer_item, _ = proxy_played(s, vuid, tok, "[Movies2]")
        if src_item and peer_item:
            break
        time.sleep(1)
    assert src_item and peer_item, "could not find both NOTLD copies via proxy"
    movies_id = src_item["Id"]
    print(f"source [Movies] id={movies_id}")
    print(f"peer   [Movies2] id={peer_item['Id']}")

    # Reset baseline: clear played on movies2 directly so we start clean.
    m2it = movies2_notld(m2tok, m2uid)
    assert m2it, "NOTLD not found on movies2 backend"
    requests.delete(f"{MOVIES2}/Users/{m2uid}/PlayedItems/{m2it['Id']}", headers=hdr(m2tok, DEVID2))
    base = movies2_played(m2tok, m2uid)
    print(f"baseline movies2 Played = {base}")
    assert base in (False, None), f"baseline not clean: {base}"

    # --- 1) Mark the Movies copy PLAYED via the proxy ---
    r = s.post(f"{PROXY}/Users/{vuid}/PlayedItems/{movies_id}", headers=hdr(tok))
    assert r.status_code in (200, 204), f"mark played {r.status_code}: {r.text[:160]}"
    print("marked [Movies] copy played via proxy; waiting for fan-out...")

    synced = wait_until(lambda: movies2_played(m2tok, m2uid), True)
    proxy_view = proxy_played(s, vuid, tok, "[Movies2]")[1]
    print(f"  movies2 backend Played = {movies2_played(m2tok, m2uid)} (ground truth)")
    print(f"  proxy [Movies2] Played = {proxy_view}")
    if not synced:
        print("FAIL: played state did NOT sync to Movies2"); sys.exit(1)
    print("PASS: played synced to Movies2 ✅")

    # --- 2) Mark the Movies copy UNPLAYED via the proxy ---
    r = s.delete(f"{PROXY}/Users/{vuid}/PlayedItems/{movies_id}", headers=hdr(tok))
    assert r.status_code in (200, 204), f"mark unplayed {r.status_code}"
    print("marked [Movies] copy unplayed via proxy; waiting for fan-out...")
    unsynced = wait_until(lambda: movies2_played(m2tok, m2uid), False)
    print(f"  movies2 backend Played = {movies2_played(m2tok, m2uid)} (ground truth)")
    if not unsynced:
        print("FAIL: unplayed state did NOT sync to Movies2"); sys.exit(1)
    print("PASS: unplayed synced to Movies2 ✅")
    print("\nALL PASS — cross-server watched-state sync works")


if __name__ == "__main__":
    main()
