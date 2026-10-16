# End-to-end tests

Real Jellyfin servers, real identity providers and real jellyfin-web in headless
Chromium, all on a Docker network with no route out (`internal: true`). The
proxy under test is a binary mounted into a plain Linux container.

## Quick start

```sh
cd e2e
./build.sh WORKTREE new          # Linux; on Apple Silicon: ./build-macos.sh WORKTREE new
./build.sh 7092b79 old           # pre-upgrade build, for the upgrade/backup suites
docker compose -p jsw-e2e build browser  # once, for the browser suites
PULL_POLICY=missing ./run-all.sh # first run pulls images; later runs work offline
```

`run-all.sh [quick|browser|sso|all]` prints `PASS`/`FAIL` lines, writes the full
log to `state/run-all.log` and exits non-zero on any failure. It generates the
test media (`make-media.sh`) and a throwaway TLS CA for Authelia
(`gen-certs.sh`) on first use.

Images are never pulled unless `PULL_POLICY=missing`. Override any of them with
`JF_A` / `JF_B` (Jellyfin tags, default `12.2` and `10.11.9`, a mixed setup),
`PROXY_BASE_IMAGE` and `RUNNER_IMAGE`.

## Suites

| Script | What it checks |
|---|---|
| `run.sh new compat12.py run_tests.py` | Jellyfin 12 API compatibility; earlier audit fixes |
| `run.sh new bypass.py` | Existing users' local passwords are checked |
| `run.sh new security.py` | Audit regressions: anonymous and cross-user access, login limits, open redirect, leaks, path traversal, deep paging |
| `run_hardening.sh` | `/servers` needs a session; `public_url` pins the SSO redirect |
| `run_startup.sh` | First requests after start/restart see every server |
| `run_paging.sh` | 230-film merged library: paging, sort, Latest, Continue Watching |
| `run_paging.sh new filters.py` | Filter values and genres combined across servers |
| `run_backup.sh` | Upgrade from `old` backs up DB + config; restoring brings `old` back |
| `run_cf.sh` | Rate limiting per `CF-Connecting-IP` behind a trusted proxy |
| `run_ui.sh` | jellyfin-web: login, library, item pages, playback, server switcher |
| `run_ui_big.sh` | jellyfin-web: paging, filter menu, Genres view on the big library |
| `run_upgrade.sh` | `old` data -> `new`: credential migration, SSO with Keycloak, Authelia, Dex and Authentik, restart |
| `run_config.sh` | Corrupt config and `session_key` reload rules (after `run_upgrade.sh`) |

Each script brings up the containers it needs and tears them down. The SSO
suites need the identity providers started and configured first; `run-all.sh sso`
does that.

## Upgrade matrix

`run_migrations.py [name ...]` upgrades 26 scenarios to `bin/new`: one source
version per schema level of the fork (February to October 2026, plus the
abandoned `rebuild-on-upstream` branch), up to 150 users and 50,000 media ids,
unusual names and passwords, damaged rows (corrupt or stale credentials,
duplicate URLs, a removed server, foreign migrations, an unclean shutdown),
config shapes (comments, key in the file, key in the environment, file lost),
a backend offline during the upgrade, SSO links, and a multi-hop upgrade. Each
checks the backup, that no user, SSO identity or media id is lost, sessions,
passwords, per-user servers, web UI and SSO logins, that no plaintext secret is
left in the database file, restart, and rollback to the old build.

Build the sources first (`bin/<name>` for each entry of `S` in the script), e.g.
`./build-macos.sh ff1de26 src-ff1de26`. It takes about two hours.

## Notes

- Servers count as offline until their first health check, so a few tests wait
  before their first request; `run_startup.sh` is the one that must not.
- The login rate limiter allows a burst of 5, then one every 10 seconds per
  client. Suites that log in a lot get a fresh stack.
- A container browser locale of `en-US@posix` crashes jellyfin-web 12.2 itself;
  the browser tests use `en-US`.
- `db_tool.py` reads the SQLite file from the host for the upgrade suite; it only
  runs while the proxy is stopped (WAL across a bind mount).
