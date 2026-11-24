"""Stdlib HTTP API server demonstrating timed profiling (`fnr scan --duration 2 -- python3 server.py`)
and finite benchmark mode (`python3 server.py --bench`).
"""

import json
import sys
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from scoring import score_candidates

CATALOG = [
    {"id": i, "name": f"rust-worker-node-{i % 47}", "tags": f"compute batching queue-{i % 19}", "weight": 1.0 + (i % 5) * 0.25}
    for i in range(450)
]
QUERY = ["worker", "batch", "latency", "compute"]


class Handler(BaseHTTPRequestHandler):
    def do_POST(self) -> None:
        length = int(self.headers.get("Content-Length", "0"))
        payload = json.loads(self.rfile.read(length) or b"{}")
        ranked = score_candidates(payload.get("records", CATALOG), payload.get("terms", QUERY))
        body = json.dumps({"top": ranked}).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, _format: str, *_args: object) -> None:
        return


def drive_requests(port: int, count: int | None = None) -> None:
    url = f"http://127.0.0.1:{port}/score"
    payload = json.dumps({"records": CATALOG, "terms": QUERY}).encode("utf-8")
    sent = 0
    while count is None or sent < count:
        req = urllib.request.Request(url, data=payload, headers={"Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=2.0) as resp:
                resp.read()
        except OSError:
            break
        sent += 1
        time.sleep(0.002)


def run_server(bench_requests: int | None = None) -> None:
    srv = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    port = srv.server_address[1]
    t = threading.Thread(target=srv.serve_forever, daemon=True)
    t.start()
    try:
        drive_requests(port, count=bench_requests)
    finally:
        srv.shutdown()
        srv.server_close()


if __name__ == "__main__":
    if "--bench" in sys.argv:
        t0 = time.perf_counter()
        run_server(bench_requests=30)
        print(f"processed 30 HTTP scoring requests in {(time.perf_counter() - t0) * 1000:.1f}ms")
    else:
        run_server(bench_requests=None)
