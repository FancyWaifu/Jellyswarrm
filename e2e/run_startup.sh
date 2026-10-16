#!/bin/sh
# run_startup.sh <bin>: query immediately after a fresh start and after a restart.
E=$(cd "$(dirname "$0")" && pwd)
export BIN=${1:-new} DATA=start
dc() { docker compose -f $E/compose.yml -p jsw-e2e "$@"; }
dc down -v >/dev/null 2>&1
rm -rf $E/data-start; mkdir -p $E/data-start; cp $E/tests/jellyswarrm.toml $E/data-start/
dc up -d jf-a jf-b runner >/dev/null 2>&1
dc exec -T runner sh -c 'cd /tests && python3 setup_backends.py' >/dev/null || exit 1
dc up -d proxy >/dev/null 2>&1
dc exec -T runner sh -c 'cd /tests && python3 startup.py fresh'
dc restart proxy >/dev/null 2>&1
dc exec -T runner sh -c 'cd /tests && python3 startup.py restart'
dc down -v >/dev/null 2>&1
