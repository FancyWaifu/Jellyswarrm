#!/bin/sh
# run_ui_big.sh <bin>: jellyfin-web against the big merged library.
E=$(cd "$(dirname "$0")" && pwd)
export BIN=${1:-new} DATA=big MEDIA=./media-big
dc() { docker compose -f $E/compose.yml -p jsw-e2e "$@"; }
dc down -v >/dev/null 2>&1
rm -rf $E/data-big; mkdir -p $E/data-big; cp $E/tests/jellyswarrm.toml $E/data-big/
dc up -d jf-a jf-b runner browser >/dev/null 2>&1
dc exec -T runner sh -c 'cd /tests && python3 setup_backends.py' >/dev/null || exit 1
dc up -d proxy >/dev/null 2>&1
dc exec -T browser sh -c 'cd /tests && python3 ui_big.py'
