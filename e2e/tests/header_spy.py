"""Relay in front of jf-b that records the headers of every request it receives
(/state/spy.jsonl). usage: header_spy.py <port>"""
import json, sys, threading, urllib.error, urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT, REAL, LOG = int(sys.argv[1]), "http://jf-b:8096", "/state/spy.jsonl"
lock = threading.Lock()
SKIP = {"host", "connection", "transfer-encoding", "content-length"}

class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a):
        pass
    def relay(self, method):
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n) if n else None
        with lock, open(LOG, "a") as f:
            f.write(json.dumps({"m": method, "p": self.path, "h": {k.lower(): v for k, v in self.headers.items()}}) + "\n")
        req = urllib.request.Request(REAL + self.path, data=body, method=method,
                                     headers={k: v for k, v in self.headers.items() if k.lower() not in SKIP})
        try:
            resp = urllib.request.urlopen(req, timeout=60)
        except urllib.error.HTTPError as e:
            resp = e
        data = resp.read()
        self.send_response(resp.status if hasattr(resp, "status") else resp.code)
        for k, v in resp.headers.items():
            if k.lower() not in SKIP:
                self.send_header(k, v)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def do_GET(self): self.relay("GET")
    def do_POST(self): self.relay("POST")
    def do_DELETE(self): self.relay("DELETE")
    def do_HEAD(self): self.relay("HEAD")

ThreadingHTTPServer(("0.0.0.0", PORT), H).serve_forever()
