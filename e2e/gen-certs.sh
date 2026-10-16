#!/bin/sh
# gen-certs.sh: throwaway test CA, Authelia TLS cert and OIDC signing key (idp/authelia).
# Regenerates when missing or within a day of expiry. Never use these anywhere else.
set -e
cd "$(dirname "$0")/idp/authelia"
if [ -f ca.pem ] && openssl x509 -checkend 86400 -noout -in ca.pem >/dev/null 2>&1 && [ -f configuration.yml ]; then exit 0; fi
openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.pem -days 30 -subj "/CN=jsw e2e test CA" 2>/dev/null
openssl req -newkey rsa:2048 -nodes -keyout tls.key -out tls.csr -subj "/CN=auth.jsw.test" 2>/dev/null
openssl x509 -req -in tls.csr -CA ca.pem -CAkey ca.key -CAcreateserial -out tls.pem -days 30 -extfile ext.cnf 2>/dev/null
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out oidc.pem 2>/dev/null
# Authelia runs as an unprivileged user and reads these from a read-only mount.
chmod 644 ca.pem tls.pem tls.key oidc.pem
python3 - <<'PY'
key = "".join("          " + l + "\n" for l in open("oidc.pem").read().strip().splitlines())
open("configuration.yml", "w").write(open("configuration.yml.in").read().replace("__OIDC_KEY__\n", key))
PY
echo "generated test CA and Authelia keys"
