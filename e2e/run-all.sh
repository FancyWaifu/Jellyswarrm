#!/bin/sh
# run-all.sh [quick|browser|sso|all]: run suites against bin/new (and bin/old where an
# upgrade is tested). Prints PASS/FAIL lines; exits non-zero on any failure.
#   quick   API, security, hardening, startup, paging, filters, upgrade backup/rollback, CF rate limits
#   browser jellyfin-web in headless Chromium (small and 230-film libraries, SSO hand-off)
#   sso     upgrade old -> new with 4 identity providers, then config failure modes
E=$(cd "$(dirname "$0")" && pwd)
GROUP=${1:-all}
LOG=$E/state/run-all.log; mkdir -p "$E/state"; : > "$LOG"
"$E/gen-certs.sh"; "$E/make-media.sh"
for b in new old; do [ -x "$E/bin/$b" ] || { echo "missing bin/$b; see README.md (build.sh)"; exit 2; }; done
dc() { docker compose -f "$E/compose.yml" -p jsw-e2e "$@"; }
suite() { echo "##### $*" | tee -a "$LOG"; "$@" 2>&1 | tee -a "$LOG" | grep -E '^(PASS|FAIL|###|=====)|passed$|Traceback|Error'; }
case $GROUP in quick|all)
  # bypass gets its own stack: the suites before it spend the login rate-limit budget.
  suite "$E/run.sh" new compat12.py run_tests.py
  suite "$E/run.sh" new bypass.py
  suite "$E/run.sh" new security.py
  suite "$E/run_hardening.sh"
  suite "$E/run_startup.sh"
  suite "$E/run_paging.sh"
  suite "$E/run_paging.sh" new filters.py
  suite "$E/run_backup.sh"
  suite "$E/run_cf.sh";;
esac
case $GROUP in browser|all)
  suite "$E/run_ui.sh"
  suite "$E/run_ui_big.sh"
  suite "$E/run_ui_sso.sh";;
esac
case $GROUP in sso|all)
  dc down -v >/dev/null 2>&1
  dc up -d jf-a jf-b runner keycloak dex ak-db ak-redis authentik ak-worker authelia >/dev/null 2>&1
  DATA=upg BIN=new dc exec -T runner sh -c 'cd /tests && python3 setup_idps.py' >/dev/null || echo "FAIL  identity providers did not come up" | tee -a "$LOG"
  suite "$E/run_upgrade.sh"
  suite "$E/run_config.sh";;
esac
dc down -v >/dev/null 2>&1
fails=$(grep -cE '^FAIL|Traceback' "$LOG")
passes=$(grep -c '^PASS' "$LOG")
echo "== $passes passed, $fails failed (full log: state/run-all.log)"
[ "$fails" = 0 ] && [ "$passes" -gt 0 ]
