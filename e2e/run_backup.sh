#!/bin/sh
# run_backup.sh <new bin>: old build data -> upgrade (backup made) -> restart (no new backup)
# -> restore backup -> old build works again; plus: a fresh DB makes no backup.
E=$(cd "$(dirname "$0")" && pwd)
NEW=${1:-new}; D=$E/data-bk
export DATA=bk
dc() { docker compose -f $E/compose.yml -p jsw-e2e "$@"; }
run() { dc exec -T runner sh -c "cd /tests && python3 backup.py $1 $2" | grep -E 'PASS|FAIL'; }
ok() { if eval "$2"; then echo "PASS  $1"; else echo "FAIL  $1"; fi; }
dc down -v >/dev/null 2>&1
rm -rf $D; mkdir -p $D; cp $E/tests/jellyswarrm.toml $D/
dc up -d jf-a jf-b runner >/dev/null 2>&1
dc exec -T runner sh -c 'cd /tests && python3 setup_backends.py' >/dev/null || exit 1
BIN=old dc up -d proxy >/dev/null 2>&1; run old "old build"
BIN=old dc stop proxy >/dev/null 2>&1
ok "old build made no backup" '[ ! -d $D/backups ]'
cp $D/jellyswarrm.toml $E/state/toml-before-upgrade

BIN=$NEW dc up -d --force-recreate proxy >/dev/null 2>&1; run check "new build after upgrade"
dc logs proxy 2>&1 | sed 's/\x1b\[[0-9;]*m//g' | grep -o 'Applying .*' | head -1 | sed 's/^/  log: /'
B1=$(ls -d $D/backups/pre-upgrade-* 2>/dev/null)
ok "one backup dir with the database and config" '[ $(echo "$B1" | grep -c .) = 1 ] && [ -s "$B1/jellyswarrm.db" ] && [ -s "$B1/jellyswarrm.toml" ]'
diff $E/state/toml-before-upgrade "$B1/jellyswarrm.toml" | grep '^[<>]' | sed 's/session_key = .*/session_key = <redacted>/;s/^/  config diff: /'
ok "backed-up config is the pre-upgrade file byte for byte" '[ -z "$(diff $E/state/toml-before-upgrade "$B1/jellyswarrm.toml" | grep "^[<>]" )" ]'
ok "live config is the original plus a session_key line (comments and layout kept)" '[ "$(tail -n +4 $D/jellyswarrm.toml)" = "$(cat $E/state/toml-before-upgrade)" ] && sed -n 2p $D/jellyswarrm.toml | grep -q "^session_key = "'
ok "backed-up database is at the old schema" '[ $(python3 -c "import sqlite3,sys;print(sqlite3.connect(sys.argv[1]).execute(\"select count(*) from _sqlx_migrations\").fetchone()[0])" "$B1/jellyswarrm.db") -lt $(python3 -c "import sqlite3,sys;print(sqlite3.connect(sys.argv[1]).execute(\"select count(*) from _sqlx_migrations\").fetchone()[0])" "$D/jellyswarrm.db") ]'

BIN=$NEW dc restart proxy >/dev/null 2>&1; run check "new build after restart"
ok "restart on migrated data made no second backup" '[ $(ls -d $D/backups/pre-upgrade-* | grep -c .) = 1 ]'

BIN=$NEW dc stop proxy >/dev/null 2>&1
cp "$B1/jellyswarrm.db" "$B1/jellyswarrm.toml" $D/; rm -f $D/jellyswarrm.db-wal $D/jellyswarrm.db-shm
BIN=old dc up -d --force-recreate proxy >/dev/null 2>&1; run check "old build on the restored backup"
BIN=old dc stop proxy >/dev/null 2>&1

rm -rf $D; mkdir -p $D; cp $E/tests/jellyswarrm.toml $D/
BIN=$NEW dc up -d --force-recreate proxy >/dev/null 2>&1; sleep 6
ok "fresh database made no backup" '[ ! -d $D/backups ]'
dc down -v >/dev/null 2>&1
