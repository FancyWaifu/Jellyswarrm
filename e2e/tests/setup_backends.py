"""Run the Jellyfin startup wizard on both backends and index /media/movies."""
import time
from lib import req, wait, hdr

for host in ("http://jf-a:8096", "http://jf-b:8096"):
    wait(f"{host}/System/Info/Public")
    # The wizard endpoints can 503 briefly while Jellyfin finishes starting.
    for _ in range(90):  # Jellyfin can answer once, then restart its listener while starting
        try:
            if req("GET", f"{host}/Startup/Configuration")[0] == 200:
                break
        except Exception:
            pass
        time.sleep(2)
    req("POST", f"{host}/Startup/Configuration",
        {"UICulture": "en-US", "MetadataCountryCode": "US", "PreferredMetadataLanguage": "en"})
    req("GET", f"{host}/Startup/User")
    req("POST", f"{host}/Startup/User", {"Name": "admin", "Password": "password"})
    req("POST", f"{host}/Startup/RemoteAccess", {"EnableRemoteAccess": True, "EnableAutomaticPortMapping": False})
    req("POST", f"{host}/Startup/Complete")

    for _ in range(30):  # right after the wizard the server may still be settling
        try:
            st, auth = req("POST", f"{host}/Users/AuthenticateByName",
                           {"Username": "admin", "Pw": "password"}, hdr("setup"))
        except Exception as e:
            st, auth = None, e
        if st == 200:
            break
        time.sleep(2)
    assert st == 200, (host, st, auth)
    h = hdr("setup", auth["AccessToken"])
    st, _ = req("POST", f"{host}/Library/VirtualFolders?name=Movies&collectionType=movies&refreshLibrary=true",
                {"LibraryOptions": {"PathInfos": [{"Path": "/media/movies"}]}}, h)
    assert st in (200, 204), (host, st)
    for i in range(90):
        st, items = req("GET", f"{host}/Items?Recursive=true&IncludeItemTypes=Movie&Fields=ProviderIds", headers=h)
        if st == 200 and items["TotalRecordCount"] >= 2:
            break
        if i % 10 == 9:  # a first scan can race library creation; scan again
            req("POST", f"{host}/Library/Refresh", headers=h)
        time.sleep(2)
    else:
        raise SystemExit(f"{host}: library never reached 2 movies")
    print(host, [(i["Name"], i.get("ProductionYear"), i.get("ProviderIds")) for i in items["Items"]])
