"""usage: backup.py old|check — create alice under the old build / check she still works."""
import sys, time
from lib import *
wait(f"{P}/System/Info/Public"); time.sleep(40)  # old builds need the first health check
if sys.argv[1] == "old":
    for host in (A, B):
        jf_create_user(host, "alice", "alicepw")
st, a = api_login(P, "alice", "alicepw", "bk-dev")
check(f"{sys.argv[2]}: alice logs in", st == 200, f"status={st}")
if st == 200:
    check(f"{sys.argv[2]}: alice sees both backends", *both_backends(a["AccessToken"], a["User"]["Id"], "bk-dev"))
summary()
