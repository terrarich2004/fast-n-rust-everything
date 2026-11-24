# HTTP Scoring API Example (Python stdlib)

Shows how to profile a live HTTP server with `--duration` (without changing how the server loops) and isolate the CPU-heavy scoring function (`scoring.score_candidates`) away from network/HTTP I/O.

## Try timed profiling (server loop)
```sh
fnr scan --duration 2 -- python3 server.py
```

## Try finite benchmark mode
```sh
fnr scan -- python3 server.py --bench
```
