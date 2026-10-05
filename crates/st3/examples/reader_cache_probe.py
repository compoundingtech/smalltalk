"""Separate client process so its CPU and allocations are excluded from daemon measurements."""
import concurrent.futures
import http.client
import json
import socket
import sys
import threading
import time

path, route = sys.argv[1:]
barrier = threading.Barrier(16)


class UnixConnection(http.client.HTTPConnection):
    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(path)


def worker(_):
    client = UnixConnection("localhost", timeout=60)
    latency = []
    barrier.wait()
    try:
        for _ in range(20):
            started = time.perf_counter()
            client.request("GET", route)
            response = client.getresponse()
            body = response.read()
            if response.status != 200:
                raise RuntimeError(f"HTTP {response.status}: {body[:300]!r}")
            latency.append((time.perf_counter() - started) * 1000)
    finally:
        client.close()
    return latency


with concurrent.futures.ThreadPoolExecutor(max_workers=16) as executor:
    samples = sorted(value for batch in executor.map(worker, range(16)) for value in batch)
print(json.dumps({"requests": len(samples), "p50_ms": samples[len(samples) // 2],
                  "p95_ms": samples[len(samples) * 95 // 100]}))
