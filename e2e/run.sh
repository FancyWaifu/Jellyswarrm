#!/bin/sh
# run.sh <bin> <test.py>...: fresh stack with ./bin/<bin>, data in ./data-<bin>, run tests in order.
cd "$(dirname "$0")"
export BIN=$1; shift
docker compose -p jsw-e2e down -v >/dev/null 2>&1
rm -rf data-$BIN && mkdir -p data-$BIN && cp tests/jellyswarrm.toml data-$BIN/
docker compose -p jsw-e2e up -d jf-a jf-b runner >/dev/null 2>&1
docker compose -p jsw-e2e exec -T runner sh -c 'cd /tests && python3 setup_backends.py' || exit 1
docker compose -p jsw-e2e up -d proxy >/dev/null 2>&1
for t in "$@"; do echo "### $BIN: $t"; docker compose -p jsw-e2e exec -T runner sh -c "cd /tests && python3 $t"; done
