#!/bin/sh
# run_headers.sh <bin>: client-only headers (admin session cookie, Referer, forwarding,
# cf-*) never reach a backend; B is reached through a header-recording relay.
E=$(cd "$(dirname "$0")" && pwd)
export BIN=${1:-new} DATA=headers
dc() { docker compose -f $E/compose.yml -p jsw-e2e "$@"; }
dc down -v >/dev/null 2>&1
rm -rf $E/data-headers $E/state/spy.jsonl; mkdir -p $E/data-headers
sed 's|url = "http://jf-b:8096"|url = "http://runner:8099"|' $E/tests/jellyswarrm.toml > $E/data-headers/jellyswarrm.toml
dc up -d jf-a jf-b runner >/dev/null 2>&1
dc exec -T runner sh -c 'cd /tests && python3 setup_backends.py' >/dev/null || exit 1
dc exec -d runner sh -c 'cd /tests && python3 header_spy.py 8099'
dc up -d proxy >/dev/null 2>&1
dc exec -T runner sh -c 'cd /tests && python3 -u headers.py'
dc down -v >/dev/null 2>&1
