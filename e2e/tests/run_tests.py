"""E2E checks against the proxy. Each check prints PASS/FAIL against the FIXED
behaviour, so the baseline binary is expected to FAIL the bug checks."""
import time
from lib import req, wait, hdr

P = "http://proxy:3000"
B = "http://jf-b:8096"
results = []


def check(name, ok, detail=""):
    results.append(ok)
    print(f"{'PASS' if ok else 'FAIL'}  {name}  {detail}")


wait(f"{P}/System/Info/Public")
time.sleep(40)  # the proxy marks backends healthy only after its first 30s health check

# --- login (regression: normal auth still works) ---------------------------
st, auth = req("POST", f"{P}/Users/AuthenticateByName", {"Username": "admin", "Pw": "password"}, hdr())
check("login through proxy works", st == 200, f"status={st}")
tok, uid = auth["AccessToken"], auth["User"]["Id"]
H = hdr(token=tok)

st, _ = req("GET", f"{P}/Users/{uid}/Views", headers=H)
check("authenticated /Users/{id}/Views still 200", st == 200, f"status={st}")

# --- 1. credential-less user resolution ------------------------------------
st, body = req("GET", f"{P}/Users/{uid}/Views")
check("no-auth /Users/{id}/Views rejected", st != 200, f"status={st}")
st, body = req("GET", f"{P}/UserViews?userId={uid}")
check("no-auth /UserViews?userId= rejected", st != 200, f"status={st}")

import re
st, views = req("GET", f"{P}/Users/{uid}/Views", headers=H)
lib = next(v["Id"] for v in views["Items"] if v.get("CollectionType") == "movies")
st, movies = req("GET", f"{P}/Items?ParentId={lib}&Recursive=true&IncludeItemTypes=Movie&Fields=ProviderIds&userId={uid}", headers=H)
items = movies["Items"] if st == 200 else []
by_name = {}
for i in items:
    by_name.setdefault(re.sub(r" \[[^\]]+\]$", "", i["Name"]), []).append(i)  # drop "[server]" suffix

hal = by_name.get("Halloween", [{}])[0].get("Id")
# Tokenless auth header with an attacker device + victim UserId in the body.
st, body = req("POST", f"{P}/Sessions/Playing",
               {"UserId": uid, "ItemId": hal, "PositionTicks": 0}, hdr(dev="evil-device"))
check("tokenless body UserId cannot act as user", st not in (200, 204), f"status={st}")

# --- 2. dedup: same title, different year, no provider ids -----------------
years = sorted(i.get("ProductionYear") for i in by_name.get("Halloween", []))
check("Halloween 1978 and 2018 both listed", years == [1978, 2018], f"years={years}")
check("same-IMDb 'Synced Film' still deduped to one",
      len(by_name.get("Synced Film", [])) == 1, f"count={len(by_name.get('Synced Film', []))}")

# --- 3. rewatch resume sync ------------------------------------------------
film = by_name["Synced Film"][0]["Id"]
st, b_auth = req("POST", f"{B}/Users/AuthenticateByName", {"Username": "admin", "Pw": "password"}, hdr("checker"))
BH = hdr("checker", b_auth["AccessToken"])
b_uid = b_auth["User"]["Id"]
st, b_items = req("GET", f"{B}/Items?Recursive=true&IncludeItemTypes=Movie&Fields=ProviderIds", headers=BH)
b_film = next(i["Id"] for i in b_items["Items"] if i["Name"] == "Synced Film")


def b_position():
    st, it = req("GET", f"{B}/Users/{b_uid}/Items/{b_film}", headers=BH)
    return it["UserData"]["PlaybackPositionTicks"]


def play(ticks_list):
    body = lambda t: {"ItemId": film, "PositionTicks": t, "PlaySessionId": f"e2e-{time.time()}"}
    req("POST", f"{P}/Sessions/Playing", body(0), H)
    for t in ticks_list:
        time.sleep(1)
        req("POST", f"{P}/Sessions/Playing/Progress", body(t), H)
    time.sleep(1)
    req("POST", f"{P}/Sessions/Playing/Stopped", body(ticks_list[-1]), H)


def wait_b(expect, secs=20):
    end = time.time() + secs
    pos = None
    while time.time() < end:
        pos = b_position()
        if abs(pos - expect) < 50_000_000:
            return pos
        time.sleep(1)
    return pos


play([3_000_000_000, 9_000_000_000])  # first watch to 15 min
pos = wait_b(9_000_000_000)
check("first watch: peer B resume = 15 min", abs(pos - 9_000_000_000) < 50_000_000, f"B={pos}")
play([1_800_000_000])  # rewatch from the start, stop at 3 min
pos = wait_b(1_800_000_000)
check("rewatch: peer B resume follows to 3 min", abs(pos - 1_800_000_000) < 50_000_000, f"B={pos}")

# --- 4. login rate limit can't be dodged with X-Forwarded-For ---------------
time.sleep(11)
codes = []
for n in range(8):
    st, _ = req("POST", f"{P}/Users/AuthenticateByName", {"Username": "admin", "Pw": "wrong"},
                {**hdr(), "X-Forwarded-For": f"203.0.113.{n}"})
    codes.append(st)
check("rotating X-Forwarded-For still hits 429", 429 in codes, f"codes={codes}")

# --- 5. /_debug master-password oracle is rate limited ----------------------
time.sleep(11)
st, _ = req("GET", f"{P}/_debug/viewmerge", headers={**H, "X-Debug-Key": "masterpw"})
check("debug endpoint works with right key (fresh bucket)", st == 200, f"status={st}")
for _ in range(6):
    req("GET", f"{P}/_debug/viewmerge", headers={**H, "X-Debug-Key": "guess"})
st, _ = req("GET", f"{P}/_debug/viewmerge", headers={**H, "X-Debug-Key": "masterpw"})
check("debug key guessing gets locked out", st != 200, f"status after guesses={st}")

print(f"\n{sum(results)}/{len(results)} passed")
