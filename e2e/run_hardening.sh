#!/bin/sh
# run_hardening.sh <bin>: /servers auth + public_url redirect check, without then with public_url.
E=$(cd "$(dirname "$0")" && pwd)
export BIN=${1:-new} DATA=hard
dc() { docker compose -f $E/compose.yml -p jsw-e2e "$@"; }
dc down -v >/dev/null 2>&1
rm -rf $E/data-hard; mkdir -p $E/data-hard; cp $E/tests/jellyswarrm.toml $E/data-hard/
dc up -d jf-a jf-b runner keycloak >/dev/null 2>&1
dc exec -T runner sh -c 'cd /tests && python3 setup_backends.py' >/dev/null || exit 1
dc up -d proxy >/dev/null 2>&1
echo "### no public_url"; dc exec -T runner sh -c 'cd /tests && python3 hardening.py https://evil.example/sso/callback'
dc stop proxy >/dev/null 2>&1
sed -i '' '1i\
public_url = "https://jelly.example.com/"
' $E/data-hard/jellyswarrm.toml
dc start proxy >/dev/null 2>&1
echo "### public_url set"; dc exec -T runner sh -c 'cd /tests && python3 hardening.py https://jelly.example.com/sso/callback'
dc down -v >/dev/null 2>&1
