"""Wait for Keycloak/Dex, and configure Authentik (provider, app, users) via its API."""
import time
from lib import req, wait

wait("http://dex:5556/dex/.well-known/openid-configuration", 300)
wait("http://keycloak:8080/realms/jsw/.well-known/openid-configuration", 300)
print("keycloak + dex ready")

AK = "http://authentik:9000"
H = {"Authorization": "Bearer e2e-ak-bootstrap-token"}
end = time.time() + 600
while time.time() < end:  # bootstrap token exists once the worker applied blueprints
    try:
        st, _ = req("GET", f"{AK}/api/v3/core/users/me/", headers=H, timeout=10)
        if st == 200:
            st2, flows = req("GET", f"{AK}/api/v3/flows/instances/?slug=default-provider-authorization-implicit-consent", headers=H)
            if st2 == 200 and flows["results"]:
                break
    except Exception:
        pass
    time.sleep(5)
else:
    raise SystemExit("authentik not ready")


def one(path):
    st, r = req("GET", f"{AK}/api/v3/{path}", headers=H)
    assert st == 200 and r["results"], (path, st, r)
    return r["results"][0]["pk"]


authz = one("flows/instances/?slug=default-provider-authorization-implicit-consent")
inval = one("flows/instances/?slug=default-provider-invalidation-flow")
key = one("crypto/certificatekeypairs/?name=authentik%20Self-signed%20Certificate")
st, maps = req("GET", f"{AK}/api/v3/propertymappings/provider/scope/?page_size=100", headers=H)
scopes = [m["pk"] for m in maps["results"] if m.get("scope_name") in ("openid", "email", "profile")]
assert len(scopes) == 3, maps
st, prov = req("POST", f"{AK}/api/v3/providers/oauth2/", {
    "name": "jellyswarrm", "authorization_flow": authz, "invalidation_flow": inval,
    "client_type": "confidential", "client_id": "jellyswarrm", "client_secret": "ak-secret",
    "redirect_uris": [{"matching_mode": "strict", "url": "http://proxy:3000/sso/callback"}],
    "signing_key": key, "property_mappings": scopes}, H)
assert st == 201, (st, prov)
st, app = req("POST", f"{AK}/api/v3/core/applications/", {"name": "Jellyswarrm", "slug": "jellyswarrm", "provider": prov["pk"]}, H)
assert st == 201, (st, app)
for u in ("aklinker", "aknew"):
    st, user = req("POST", f"{AK}/api/v3/core/users/", {"username": u, "name": u, "email": f"{u}@ak.test", "is_active": True, "path": "users"}, H)
    assert st == 201, (st, user)
    st, _ = req("POST", f"{AK}/api/v3/core/users/{user['pk']}/set_password/", {"password": "akpass"}, H)
    assert st == 204, st
wait(f"{AK}/application/o/jellyswarrm/.well-known/openid-configuration")
print("authentik ready")
