#!/usr/bin/env python3
"""Upgrade matrix: many source versions x data shapes x config shapes -> bin/new.

usage: ./run_migrations.py [scenario-name-substring ...]

Each scenario: an old build (bin/src-<commit>, see SOURCES) fills a fresh data dir
through its own API (tests/mig_gen.py); optional damage is applied to the database
while it's stopped; then bin/new starts on it. Checked:
  - startup backs up the database + original config, applies every migration once
  - no user, SSO identity or media id is lost; media ids still map to the same item
  - pre-upgrade sessions, passwords, wrong passwords, per-user servers, web UI, SSO
  - no plaintext password or backend token is left in the database files
  - a restart makes no second backup and everything still works
  - the backup restores and the old build runs on it again (rollback)
Build sources first (bin/<value> for each entry of S), e.g.: ./build-macos.sh ff1de26 src-ff1de26
"""
import base64, glob, json, os, random, shutil, sqlite3, subprocess, sys, time

E = os.path.dirname(os.path.abspath(__file__))
STATE = os.path.join(E, "state", "mig")
REPO_MIGRATIONS = sorted(int(f.split("_")[0]) for f in os.listdir(os.path.join(E, "..", "crates/jellyswarrm-proxy/migrations")) if f.endswith(".up.sql"))
RESULTS = []

# Source versions: the last commit at each schema level of the fork, plus GitHub main
# (ebecadf) and the abandoned rebuild-on-upstream branch.
S = {"feb26": "src-eeee397", "mar26": "src-baa1970", "apr26": "src-e520bba", "may26": "src-feed661",
     "jun26a": "src-c2ffdb4", "jun26b": "src-ff1de26", "jun26c": "src-6b8a9e7", "main": "src-ebecadf", "upstream": "src-b51ff2d"}

SCENARIOS = [
    # one per schema level, realistic data written by that version itself
    dict(name="feb26-base", src="feb26", users=12, flags="devices,watch"),
    dict(name="mar26-base", src="mar26", users=12, flags="devices,watch"),
    dict(name="apr26-base", src="apr26", users=12, flags="devices,watch"),
    dict(name="may26-base", src="may26", users=12, flags="devices,watch"),
    dict(name="jun26a-sso", src="jun26a", users=12, flags="devices,watch,sso"),
    dict(name="jun26b-sso", src="jun26b", users=12, flags="devices,watch,sso"),
    dict(name="jun26c-base", src="jun26c", users=12, flags="devices,watch,ui"),
    dict(name="main-base", src="main", users=15, flags="devices,watch,ui"),
    # volume
    dict(name="main-150-users", src="main", users=150, flags="devices"),
    dict(name="feb26-90-users", src="feb26", users=90, flags="devices"),
    dict(name="main-50k-media", src="main", users=9, flags="", damage=["bulk_media"]),
    # unusual data
    dict(name="main-odd-names", src="main", users=10, flags="odd,devices"),
    dict(name="main-dup-urls", src="main", users=9, flags="", damage=["dup_urls", "server_url_slash"]),
    dict(name="main-corrupt-creds", src="main", users=9, flags="", damage=["corrupt"]),
    dict(name="main-removed-server", src="main", users=9, flags="", damage=["remove_server_b"]),
    dict(name="main-foreign-migrations", src="main", users=9, flags="", damage=["foreign_tables"]),
    dict(name="main-unclean-shutdown", src="main", users=9, flags="devices", kill=True),
    dict(name="main-stale-backend-pw", src="main", users=9, flags="", damage=["stale_backend_pw"]),
    # config shapes
    dict(name="main-config-comments", src="main", users=6, flags="", config="comments"),
    dict(name="main-config-has-key", src="main", users=6, flags="", config="with_key"),
    dict(name="main-config-env-key", src="main", users=6, flags="", config="env_key"),
    dict(name="main-config-lost", src="main", users=3, flags="", config="no_toml"),
    # environment
    dict(name="main-backend-offline", src="main", users=9, flags="devices", down="B"),
    dict(name="main-sso-ui", src="main", users=9, flags="sso,ui,devices"),
    # history
    dict(name="multi-hop", src="mar26", users=12, flags="devices,watch", hops=["jun26b", "main"]),
    dict(name="upstream-branch", src="upstream", users=9, flags="devices", foreign=True),
]


def sh(*cmd, env=None, check=False, capture=True):
    r = subprocess.run(cmd, env={**os.environ, **(env or {})}, text=True,
                       stdout=subprocess.PIPE if capture else None, stderr=subprocess.STDOUT if capture else None)
    if check and r.returncode:
        raise SystemExit(f"failed: {' '.join(cmd)}\n{r.stdout}")
    return r


def dc(*args, bin="new", data="x", extra_files=(), env=None):
    files = ["-f", os.path.join(E, "compose.yml")] + [x for f in extra_files for x in ("-f", f)]
    return sh("docker", "compose", *files, "-p", "jsw-e2e", *args, env={"BIN": bin, "DATA": data, **(env or {})})


def runner(script, *args):
    r = dc("exec", "-T", "runner", "sh", "-c", "cd /tests && python3 -u " + " ".join([script, *[f"'{a}'" for a in args]]))
    for line in r.stdout.splitlines():
        if line.startswith(("PASS", "FAIL", "Traceback", "  info")) or "generated" in line:
            print("   ", line, flush=True)
    RESULTS.extend(l.startswith("PASS") for l in r.stdout.splitlines() if l.startswith(("PASS", "FAIL")))
    if "Traceback" in r.stdout:
        RESULTS.append(False)
        print("\n".join("      " + l for l in r.stdout.splitlines()[-6:]))
    return r.stdout


def check(name, ok, detail=""):
    RESULTS.append(bool(ok))
    print(f"    {'PASS' if ok else 'FAIL'}  {name}  {detail}", flush=True)
    return ok


def wait_proxy(bin, data, secs=90, extra_files=(), env=None):
    """Until the proxy answers, or its container exits. Returns 'up' or 'exited'."""
    end = time.time() + secs
    while time.time() < end:
        st = sh("docker", "inspect", "-f", "{{.State.Status}}", "jsw-e2e-proxy-1").stdout.strip()
        if st == "exited":
            return "exited"
        r = dc("exec", "-T", "runner", "python3", "-c",
               "import urllib.request;urllib.request.urlopen('http://proxy:3000/System/Info/Public',timeout=2)")
        if r.returncode == 0:
            return "up"
        time.sleep(1)
    return "timeout"


def start(bin, data, **kw):
    dc("rm", "-sf", "proxy", bin=bin, data=data)
    dc("up", "-d", "--no-deps", "proxy", bin=bin, data=data, **kw)
    return wait_proxy(bin, data, **{k: v for k, v in kw.items() if k in ("extra_files", "env")})


def stop(data, kill=False):
    dc("kill" if kill else "stop", "proxy", data=data)


def db_files(d):
    return [p for p in glob.glob(os.path.join(d, "jellyswarrm.db*"))]


def snapshot(d, dest):
    os.makedirs(dest, exist_ok=True)
    for p in db_files(d):
        shutil.copy2(p, dest)
    # Open a copy (applies any WAL) so dumps never touch the scenario's files.
    con = sqlite3.connect(os.path.join(dest, "jellyswarrm.db"))
    con.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    return con


def tables(con):
    return {r[0] for r in con.execute("SELECT name FROM sqlite_master WHERE type='table'")}


def dump(con):
    t = tables(con)
    out = {"users": {r[0]: r[1] for r in con.execute("SELECT id, original_username FROM users")} if "users" in t else {},
           "media": {r[0]: (r[1].replace("-", "").lower(), r[2].strip().rstrip("/")) for r in
                     con.execute("SELECT virtual_media_id, original_media_id, server_url FROM media_mappings")} if "media_mappings" in t else {},
           "migrations": {r[0] for r in con.execute("SELECT version FROM _sqlx_migrations WHERE success = 1")} if "_sqlx_migrations" in t else set(),
           "tokens": [r[0] for r in con.execute("SELECT jellyfin_token FROM authorization_sessions")] if "authorization_sessions" in t else []}
    for name in ("oidc_identities", "user_identities", "sso_identities"):
        if name in t:
            out["identities"] = con.execute(f"SELECT COUNT(*) FROM {name}").fetchone()[0]
    return out


# ------------------------------------------------------------------ damage
def damage(kind, d, sc):
    con = sqlite3.connect(os.path.join(d, "jellyswarrm.db"))
    cols = lambda t: [r[1] for r in con.execute(f"PRAGMA table_info({t})")]
    skip = []
    if kind == "bulk_media":
        now = "2026-01-01T00:00:00Z"
        rows = [(os.urandom(16).hex(), os.urandom(16).hex(), random.choice(["http://jf-a:8096", "http://jf-b:8096/"]), now) for _ in range(50000)]
        con.executemany("INSERT INTO media_mappings (virtual_media_id, original_media_id, server_url, created_at) VALUES (?,?,?,?)", rows)
    elif kind == "dup_urls":
        c = [x for x in cols("server_mappings") if x != "id"]
        n = con.execute(f"""INSERT INTO server_mappings ({','.join(c)})
            SELECT {','.join("CASE WHEN server_url LIKE '%/' THEN RTRIM(server_url, '/') ELSE server_url || '/' END" if x == 'server_url' else x for x in c)}
            FROM server_mappings LIMIT 6""").rowcount
        print(f"      duplicated {n} mappings under the other trailing-slash spelling of their URL")
        assert n > 0, "dup_urls damaged nothing"
    elif kind == "server_url_slash":
        con.execute("UPDATE servers SET url = url || '/' WHERE name = 'A'")
    elif kind == "corrupt":
        users = [r[0] for r in con.execute("SELECT u.original_username FROM users u JOIN server_mappings m ON m.user_id = u.id GROUP BY u.id LIMIT 2")]
        for u in users:
            con.execute("UPDATE server_mappings SET mapped_password = ? WHERE user_id = (SELECT id FROM users WHERE original_username = ?)",
                        (base64.b64encode(os.urandom(48)).decode(), u))
        skip += users
        print(f"      corrupted stored credentials of {users}")
    elif kind == "remove_server_b":  # the admin removed server B (config + database)
        import re
        p = os.path.join(d, "jellyswarrm.toml")
        open(p, "w").write(re.sub(r'\[\[preconfigured_servers\]\]\nurl = "http://jf-b:8096"\n(?:[^\[\n].*\n)*\n*', "", open(p).read()))
        con.execute("DELETE FROM servers WHERE name = 'B'")
    elif kind == "foreign_tables":
        con.execute("CREATE TABLE api_keys (id INTEGER PRIMARY KEY, user_id TEXT, key TEXT)")
        con.executemany("INSERT INTO api_keys (user_id, key) VALUES (?, ?)", [(f"u{i}", os.urandom(16).hex()) for i in range(50)])
        con.execute("CREATE TABLE library_groups (id INTEGER PRIMARY KEY, name TEXT)")
        for v, desc in ((20260729120000, "virtual user api keys"), (20260608120000, "add library groups")):
            con.execute("INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (?, ?, 1, ?, 1)",
                        (v, desc, os.urandom(48)))
    elif kind == "stale_backend_pw":
        names = [u["name"] for u in json.load(open(os.path.join(E, "state/mig", sc["name"] + ".json")))["users"] if u["servers"] == ["A"]][:2]
        runner_py = "from lib import *\nh=jf_admin(A)\nus={u['Name']:u['Id'] for u in req('GET',f'{A}/Users',headers=h)[1]}\n" + "".join(
            f"print(req('POST',f'{{A}}/Users/{{us[{n!r}]}}/Password',{{'NewPw':'changed-on-backend'}},h)[0])\n" for n in names)
        open(os.path.join(E, "tests", "_stale.py"), "w").write(runner_py)
        print("      backend password changed for", names, runner("_stale.py").strip()[:40])
        os.remove(os.path.join(E, "tests", "_stale.py"))
        skip += names
    con.commit()
    con.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    con.close()
    return skip


# ------------------------------------------------------------------ config
def write_config(d, variant):
    base = open(os.path.join(E, "tests", "jellyswarrm.toml")).read()
    if variant == "comments":
        base = "# Jellyswarrm config — hand edited\n\n#   comment with 'quotes' and = signs\n" + base.replace("\n[[", "\n# next block\n\n[[")
    if variant == "with_key":
        base = f'session_key = "{base64.b64encode(os.urandom(64)).decode()}"\n' + base
    open(os.path.join(d, "jellyswarrm.toml"), "w").write(base)


def prepare_upgrade_config(d, runner_ip):
    """What an operator does before upgrading: trust the reverse proxy (here the runner)."""
    p = os.path.join(d, "jellyswarrm.toml")
    if os.path.exists(p):
        s = open(p).read()
        open(p, "w").write(f'trusted_proxies = ["{runner_ip}"]\n' + s)


# ------------------------------------------------------------------ scenario
def run(sc):
    name, src = sc["name"], S[sc["src"]]
    print(f"\n##### {name}  ({src}, {sc['users']} users, flags={sc.get('flags') or '-'}, "
          f"damage={sc.get('damage', [])}, config={sc.get('config', 'default')})", flush=True)
    if not os.path.exists(os.path.join(E, "bin", src)):
        check(f"{name}: source build bin/{src} exists", False, "build it first")
        return
    data = f"mig-{name}"
    d = os.path.join(E, f"data-{data}")
    shutil.rmtree(d, ignore_errors=True); os.makedirs(d)
    sdir = os.path.join(STATE, name); shutil.rmtree(sdir, ignore_errors=True); os.makedirs(sdir)
    write_config(d, sc.get("config"))

    # 1. old build writes the data
    if start(src, data) != "up":
        check(f"{name}: old build starts", False, dc("logs", "--tail", "5", "proxy", data=data).stdout[-300:])
        return
    runner("mig_gen.py", name, str(sc["users"]), sc.get("flags", ""))
    stop(data, kill=sc.get("kill", False))
    for hop in sc.get("hops", []):
        st = start(S[hop], data)
        check(f"{name}: intermediate upgrade with {S[hop]}", st == "up", st)
        time.sleep(10)
        stop(data)

    # 2. damage, then snapshot what the upgrade starts from
    skip = []
    for kind in sc.get("damage", []):
        skip += damage(kind, d, sc)
    pre_con = snapshot(d, os.path.join(sdir, "pre"))
    pre = dump(pre_con)
    gen_path = os.path.join(STATE, name + ".json")
    if not os.path.exists(gen_path):
        check(f"{name}: data generated on the old build", False, "mig_gen.py produced nothing")
        return
    gen = json.load(open(gen_path))
    have = {}
    if "server_mappings" in tables(pre_con):
        for uname, url in pre_con.execute("SELECT u.original_username, m.server_url FROM server_mappings m JOIN users u ON u.id = m.user_id"):
            have.setdefault(uname, set()).add("A" if "jf-a" in url else "B")
    short = [u["name"] for u in gen["users"] if u.get("id") and have.get(u["name"], set()) != set(u["servers"])]
    for u in gen["users"]:
        u["servers_pre"] = sorted(have.get(u["name"], set()))
    json.dump(gen, open(gen_path, "w"), indent=1)
    if short:
        print(f"    info  the old build left {len(short)} user(s) mapped to fewer servers than they have accounts on: {short[:4]}")
    runner_ip = sh("docker", "inspect", "-f", "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}", "jsw-e2e-runner-1").stdout.strip()
    if sc.get("config") == "no_toml":
        os.remove(os.path.join(d, "jellyswarrm.toml"))
    else:
        prepare_upgrade_config(d, runner_ip)
    toml_path = os.path.join(d, "jellyswarrm.toml")
    toml_before = open(toml_path, "rb").read() if os.path.exists(toml_path) else None

    # 3. first start of the new build
    extra, env = (), None
    if sc.get("config") == "env_key":
        ov = os.path.join(sdir, "env.yml")
        open(ov, "w").write("services:\n  proxy:\n    environment:\n      JELLYSWARRM_SESSION_KEY: "
                            f"\"{base64.b64encode(os.urandom(64)).decode()}\"\n")
        extra = (ov,)
    if sc.get("down"):
        dc("pause", "jf-" + sc["down"].lower(), data=data)
    t0 = time.time()
    st = start("new", data, extra_files=extra, env=env)
    logs = dc("logs", "proxy", data=data, extra_files=extra).stdout
    check(f"{name}: new build starts on the old data", st == "up", f"{st} in {time.time() - t0:.1f}s")
    check(f"{name}: no panic in the log", "panicked" not in logs, "")
    applied = [l for l in logs.splitlines() if "Applying" in l and "migration" in l]
    if not sc.get("foreign"):
        check(f"{name}: backup announced before migrating", bool(applied), applied[0].split("WARN")[-1][:90] if applied else "")
    if st != "up":
        print("\n".join("      " + l for l in logs.splitlines()[-8:]))
        rollback(sc, d, data, src, pre, skip)
        return
    if sc.get("down"):  # users whose every server is down can't log in meanwhile; they must fail cleanly
        skip_new = skip + [u["name"] for u in gen["users"] if u.get("servers_pre") == [sc["down"]]]
    else:
        skip_new = skip
    skip_arg = "skip=" + "|".join(skip)
    down_arg = f"down={sc['down']}" if sc.get("down") else ("down=B" if "remove_server_b" in sc.get("damage", []) else "down=")
    if "remove_server_b" in sc.get("damage", []):
        skip += [u["name"] for u in json.load(open(os.path.join(STATE, name + ".json")))["users"] if u["servers"] == ["B"]]
        skip_arg = "skip=" + "|".join(skip)
    runner("mig_verify.py", name, "new", "skip=" + "|".join(skip_new), down_arg, *(["pace=1"] if sc.get("config") == "no_toml" else []))
    if sc.get("down"):
        dc("unpause", "jf-" + sc["down"].lower(), data=data)
        stop(data); start("new", data)
        runner("mig_verify.py", name, "restart", skip_arg, "down=")
    stop(data)

    # 4. what the upgrade did to the files
    backups = sorted(glob.glob(os.path.join(d, "backups", "pre-upgrade-*")))
    check(f"{name}: exactly one backup", len(backups) == 1, f"{[os.path.basename(b) for b in backups]}")
    if backups:
        b = dump(snapshot(backups[0], os.path.join(sdir, "backup")))
        check(f"{name}: backup holds the pre-upgrade database", b["migrations"] == pre["migrations"] and b["users"] == pre["users"]
              and len(b["media"]) == len(pre["media"]), f"migrations {len(b['migrations'])}={len(pre['migrations'])} users {len(b['users'])}={len(pre['users'])}")
        bt = os.path.join(backups[0], "jellyswarrm.toml")
        if toml_before is not None:
            check(f"{name}: backup holds the config byte for byte", os.path.exists(bt) and open(bt, "rb").read() == toml_before, "")
    post = dump(snapshot(d, os.path.join(sdir, "post")))
    check(f"{name}: every repo migration applied", set(REPO_MIGRATIONS) <= post["migrations"], f"missing={sorted(set(REPO_MIGRATIONS) - post['migrations'])}")
    lost = set(pre["users"]) - set(post["users"])
    check(f"{name}: no user lost", not lost, f"{len(post['users'])} users, lost={[pre['users'][u] for u in lost][:5]}")
    moved = [v for v, o in pre["media"].items() if v not in post["media"] or post["media"][v][0] != o[0]]
    check(f"{name}: every media id still points at the same item", not moved, f"{len(pre['media'])} ids, changed={len(moved)}")
    if "identities" in pre:
        check(f"{name}: SSO identities kept", post.get("identities", 0) >= pre["identities"], f"{pre['identities']} -> {post.get('identities')}")
    blob = b"".join(open(p, "rb").read() for p in db_files(d))
    leaked = [u["name"] for u in gen["users"] if u["name"] not in skip and len(u["pw"]) >= 6 and u["pw"].encode() in blob]
    leaked_tokens = [t[:6] for t in pre["tokens"] if not t.startswith("enc1:") and len(t) >= 16 and t.encode() in blob]
    check(f"{name}: no plaintext password left in the database", not leaked, f"leaked={leaked[:4]}")
    check(f"{name}: no plaintext backend token left in the database", not leaked_tokens, f"leaked={leaked_tokens[:4]}")
    if os.path.exists(toml_path) and toml_before is not None:
        after = open(toml_path, "rb").read()
        if sc.get("config") in ("with_key", "env_key"):
            check(f"{name}: config untouched (key already provided)", after == toml_before, "")
        else:
            check(f"{name}: config only gained a session_key line", after.endswith(toml_before) and b"session_key" in after[: len(after) - len(toml_before)], "")
    elif sc.get("config") == "no_toml":
        check(f"{name}: a lost config is recreated with a session_key", os.path.exists(toml_path) and b"session_key" in open(toml_path, "rb").read(), "")

    # 5. restart
    st = start("new", data, extra_files=extra)
    runner("mig_verify.py", name, "restart", skip_arg, down_arg, *(["pace=1"] if sc.get("config") == "no_toml" else []))
    stop(data)
    check(f"{name}: restart made no second backup", len(glob.glob(os.path.join(d, "backups", "pre-upgrade-*"))) == len(backups), "")
    rollback(sc, d, data, src, pre, skip)


def rollback(sc, d, data, src, pre, skip=()):
    name = sc["name"]
    backups = sorted(glob.glob(os.path.join(d, "backups", "pre-upgrade-*")))
    if not backups:
        check(f"{name}: rollback possible", sc.get("foreign", False), "no backup made")
        return
    last = sc["hops"][-1] if sc.get("hops") else None
    old = S[last] if last else src
    for p in db_files(d):
        os.remove(p)
    for f in ("jellyswarrm.db", "jellyswarrm.toml"):
        if os.path.exists(os.path.join(backups[0], f)):
            shutil.copy2(os.path.join(backups[0], f), d)
    st = start(old, data)
    check(f"{name}: rollback — {old} starts on the restored backup", st == "up", st)
    if st == "up":
        runner("mig_verify.py", name, "rollback", "skip=" + "|".join(skip), "down=B" if "remove_server_b" in sc.get("damage", []) else "down=")
    stop(data)


def main():
    picks = [s for s in SCENARIOS if not sys.argv[1:] or any(a in s["name"] for a in sys.argv[1:])]
    os.makedirs(STATE, exist_ok=True)
    sh(os.path.join(E, "gen-certs.sh")); sh(os.path.join(E, "make-media.sh"))
    dc("down", "-v")
    dc("up", "-d", "jf-a", "jf-b", "runner", "keycloak", "dex")
    dc("exec", "-T", "runner", "sh", "-c", "cd /tests && python3 setup_backends.py")
    per = {}
    for sc in picks:
        before = len(RESULTS)
        try:
            run(sc)
        except Exception as e:  # report and carry on with the next scenario
            check(f"{sc['name']}: scenario ran to completion", False, repr(e)[:200])
            dc("rm", "-sf", "proxy")
        res = RESULTS[before:]
        per[sc["name"]] = (sum(res), len(res))
    dc("down", "-v")
    print("\n===== summary")
    for n, (p, t) in per.items():
        print(f"  {'ok  ' if p == t else 'FAIL'}  {n:28} {p}/{t}")
    print(f"== {sum(RESULTS)} passed, {len(RESULTS) - sum(RESULTS)} failed, {len(per)} scenarios")
    sys.exit(0 if all(RESULTS) and RESULTS else 1)


if __name__ == "__main__":
    main()
