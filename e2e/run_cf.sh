#!/bin/sh
E=$(cd "$(dirname "$0")" && pwd)
# run_cf.sh [bin]: per-client rate limiting from CF-Connecting-IP via a trusted proxy.
dc() { DATA=cf BIN=${BIN:-new} docker compose -f $E/compose.yml -p jsw-e2e "$@"; }
BIN=${1:-new}
dc down -v >/dev/null 2>&1
dc up -d jf-a jf-b runner >/dev/null 2>&1
dc exec -T runner sh -c 'cd /tests && python3 setup_backends.py' >/dev/null || exit 1
RIP=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' jsw-e2e-runner-1)
rm -rf $E/data-cf; mkdir -p $E/data-cf
python3 - "$E/tests/jellyswarrm.toml" "$E/data-cf/jellyswarrm.toml" "$RIP" <<'PY'
import sys; s = open(sys.argv[1]).read()
s = s.replace("oidc_auto_provision = true\n", f'oidc_auto_provision = true\ntrusted_proxies = ["{sys.argv[3]}"]\ntrust_cf_connecting_ip = true\n', 1)
open(sys.argv[2], "w").write(s)
PY
echo "runner (trusted tunnel) = $RIP"
dc up -d --force-recreate proxy >/dev/null 2>&1; dc exec -T runner sh -c 'cd /tests && python3 cf.py'
dc down -v >/dev/null 2>&1
