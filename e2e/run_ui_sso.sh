#!/bin/sh
# run_ui_sso.sh [bin]: SSO (Keycloak) in Chromium through to the jellyfin-web 12.2 home page.
E=$(cd "$(dirname "$0")" && pwd)
export BIN=${1:-new} DATA=uisso
dc() { docker compose -f "$E/compose.yml" -p jsw-e2e "$@"; }
dc down -v >/dev/null 2>&1
rm -rf "$E/data-uisso"; mkdir -p "$E/data-uisso"; cp "$E/tests/jellyswarrm.toml" "$E/data-uisso/"
dc up -d jf-a jf-b runner browser keycloak >/dev/null 2>&1
dc exec -T runner sh -c 'cd /tests && python3 setup_backends.py' >/dev/null || exit 1
dc up -d proxy >/dev/null 2>&1
dc exec -T browser sh -c 'cd /tests && python3 -u ui_sso.py'
