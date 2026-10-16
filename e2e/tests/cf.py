"""Rate limiting keyed on CF-Connecting-IP from a trusted tunnel (the runner)."""
import time
from lib import *
wait(f"{P}/System/Info/Public")
time.sleep(12)  # fresh bucket
def attempt(cf_ip):
    return req("POST", f"{P}/Users/AuthenticateByName", {"Username": "nobody", "Pw": "x"},
               {**hdr(), "CF-Connecting-IP": cf_ip})[0]
codes = [attempt(f"203.0.113.{n}") for n in range(8)]
check("distinct CF client IPs each get their own bucket", 429 not in codes, f"codes={codes}")
codes = [attempt("198.51.100.7") for _ in range(7)]
check("one CF client IP is limited after its burst", codes[:5].count(429) == 0 and 429 in codes[5:], f"codes={codes}")
summary()
