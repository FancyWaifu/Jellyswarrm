#!/bin/sh
# Upgrade test: old binary builds legacy state -> new binary on the same data -> SSO -> restarts.
# DB reports are taken only while the proxy is stopped (SQLite WAL isn't readable
# from another process across the Docker bind mount while the proxy holds it).
E=$(cd "$(dirname "$0")" && pwd)
D=$E/data-upg
dc() { DATA=upg docker compose -f $E/compose.yml -p jsw-e2e "$@"; }
run() { dc exec -T runner sh -c "cd /tests && python3 $1"; }
report() { echo "--- DB: $1"; python3 $E/db_tool.py report $D; }
SECRETS="alicepw bobjfpass frankjfpw frankpass carolpw 0123456789abcdef0123456789abcdef01234567"

dc rm -sf proxy >/dev/null 2>&1
# Fresh Jellyfin backends each run: leftover accounts would (correctly) block SSO provisioning.
BIN=old dc up -d --force-recreate jf-a jf-b >/dev/null 2>&1
run setup_backends.py >/dev/null || exit 1
rm -rf $D; mkdir -p $D $E/state; rm -f $E/state/old.json $E/state/new.json $E/state/sso.json; cp $E/tests/jellyswarrm.toml $D/

echo "===== OLD binary"; BIN=old dc up -d proxy >/dev/null 2>&1; run phase_old.py
BIN=old dc stop proxy >/dev/null 2>&1; report "written by old binary"
OLD_TOKENS=$(python3 $E/db_tool.py tokens $D)
echo "  $(echo $OLD_TOKENS | wc -w | tr -d ' ') plaintext backend tokens recorded for the leak scan"
python3 $E/db_tool.py mutate $D; report "after rewriting erin/frank to older formats"

echo "===== NEW binary, first start on old data"
BIN=new dc up -d --force-recreate proxy >/dev/null 2>&1; sleep 8
dc logs proxy 2>&1 | sed 's/\x1b\[[0-9;]*m//g' | grep -iE "legacy|Encrypted|session_key|could not be verified|Decryption" | sed 's/^/  log: /'
BIN=new dc stop proxy >/dev/null 2>&1; report "right after startup migration (no logins yet)"
grep -q '^session_key' $D/jellyswarrm.toml && echo "  session_key persisted to config file" || echo "  session_key NOT persisted"
BIN=new dc start proxy >/dev/null 2>&1; run phase_new.py
BIN=new dc stop proxy >/dev/null 2>&1; report "after phase_new logins"
echo "===== SSO"; BIN=new dc start proxy >/dev/null 2>&1; run sso_full.py
echo "===== restart"; BIN=new dc restart proxy >/dev/null 2>&1; run phase_restart.py
BIN=new dc stop proxy >/dev/null 2>&1; report "final"
echo "--- secrets in raw DB files"; python3 $E/db_tool.py leak $D $SECRETS $OLD_TOKENS
