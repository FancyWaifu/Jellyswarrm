"""Host-side DB inspection/mutation for the upgrade test (proxy must be stopped for `mutate`)."""
import base64, hashlib, os, sqlite3, sys
from cryptography.hazmat.primitives.ciphers.aead import AESGCM

HEX40 = "0123456789abcdef0123456789abcdef01234567"
cmd, data = sys.argv[1], sys.argv[2]
db = sqlite3.connect(os.path.join(data, "jellyswarrm.db"))


def mapping_of(user):
    return db.execute("""SELECT m.id FROM server_mappings m JOIN users u ON u.id = m.user_id
                         WHERE u.original_username = ?""", (user,)).fetchone()[0]


if cmd == "mutate":
    # erin: plaintext that is also valid base64 (ambiguous); frank: oldest scheme, key = SHA256(raw password).
    db.execute("UPDATE server_mappings SET mapped_password = ? WHERE id = ?", (HEX40, mapping_of("erin")))
    nonce = os.urandom(12)
    ct = AESGCM(hashlib.sha256(b"frankpass").digest()).encrypt(nonce, b"frankjfpw", None)
    db.execute("UPDATE server_mappings SET mapped_password = ? WHERE id = ?",
               (base64.b64encode(nonce + ct).decode(), mapping_of("frank")))
    db.commit()
    db.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    print("mutated erin (plaintext hex) and frank (raw-password key)")
elif cmd == "report":
    cols = [r[1] for r in db.execute("PRAGMA table_info(server_mappings)")]
    for name, h in db.execute("SELECT original_username, original_password_hash FROM users ORDER BY 1"):
        kind = "argon2id" if h.startswith("$argon2id$") else ("sha256" if len(h) == 64 else f"other({h[:12]})")
        fmts = [r[0] for r in db.execute(
            f"SELECT {'credential_format' if 'credential_format' in cols else chr(39)+'n/a'+chr(39)} FROM server_mappings m JOIN users u ON u.id=m.user_id WHERE u.original_username=?", (name,))]
        print(f"  {name:28} verifier={kind:9} mappings={fmts}")
    toks = [r[0] for r in db.execute("SELECT jellyfin_token FROM authorization_sessions")]
    print(f"  tokens: {sum(t.startswith('enc1:') for t in toks)} encrypted, {sum(not t.startswith('enc1:') for t in toks)} plaintext")
elif cmd == "tokens":
    print(" ".join(r[0] for r in db.execute("SELECT jellyfin_token FROM authorization_sessions")))
elif cmd == "leak":
    blobs = b"".join(open(os.path.join(data, f), "rb").read() for f in os.listdir(data) if f.startswith("jellyswarrm.db"))
    for s in sys.argv[3:]:
        print(f"  {'LEAK' if s.encode() in blobs else 'clean':5} {s}")
