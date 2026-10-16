#!/bin/sh
# run_ui.sh [bin] [jf-a tag] [jf-b tag]: jellyfin-web login, library, item pages, playback, switcher.
E=$(cd "$(dirname "$0")" && pwd)
export BIN=${1:-new} JF_A=${2:-${JF_A:-12.2}} JF_B=${3:-${JF_B:-10.11.9}} DATA=ui
dc() { docker compose -f $E/compose.yml -p jsw-e2e "$@"; }
dc down -v >/dev/null 2>&1
rm -rf $E/data-ui $E/state/shots; mkdir -p $E/data-ui; cp $E/tests/jellyswarrm.toml $E/data-ui/
dc up -d jf-a jf-b runner browser >/dev/null 2>&1
dc exec -T runner sh -c 'cd /tests && python3 setup_backends.py' >/dev/null || exit 1
dc up -d proxy >/dev/null 2>&1
dc exec -T browser sh -c 'cd /tests && python3 ui_test.py'
