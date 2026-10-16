#!/bin/sh
# Config failure modes on the upgraded data dir (run after run_upgrade.sh).
E=$(cd "$(dirname "$0")" && pwd)
D=$E/data-upg; C=$D/jellyswarrm.toml
dc() { DATA=upg BIN=new docker compose -f $E/compose.yml -p jsw-e2e "$@"; }
run() { dc exec -T runner sh -c "cd /tests && python3 $*"; }
cp $C $E/state/good.toml

echo "===== corrupt config: must refuse to start (no silent defaults / new key)"
dc stop proxy >/dev/null 2>&1; printf 'port = "not a number"\n[[[broken\n' >> $C
dc up -d proxy >/dev/null 2>&1; sleep 6
st=$(docker inspect -f '{{.State.Status}} exit={{.State.ExitCode}}' jsw-e2e-proxy-1)
echo "  container: $st"; dc logs --tail 3 proxy 2>&1 | sed 's/\x1b\[[0-9;]*m//g' | grep -i "configuration" | sed 's/^/  log: /'
case "$st" in exited*) echo "PASS  corrupt config stops startup";; *) echo "FAIL  corrupt config stops startup";; esac
cp $E/state/good.toml $C; dc up -d proxy >/dev/null 2>&1; sleep 45

echo "===== reload with a different session_key: refused, running key kept"
python3 - "$C" <<'PY'
import base64, os, re, sys
p = sys.argv[1]; s = open(p).read()
open(p, "w").write(re.sub(r'(?m)^session_key = .*$', f'session_key = "{base64.b64encode(os.urandom(64)).decode()}"', s))
PY
run reload.py refused
echo "===== reload with session_key removed: keeps the running key"
grep -v '^session_key' $E/state/good.toml > $C
run reload.py ok
cp $E/state/good.toml $C
echo "===== reload with the unchanged config"
run reload.py ok
