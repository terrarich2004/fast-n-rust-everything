# HTTP Scoring API Example (Python stdlib)

Shows how to profile a live HTTP server with `--duration` (without changing how the server loops) and isolate the CPU-heavy scoring function (`scoring.score_candidates`) away from network/HTTP I/O.

## Try timed profiling (built-in self-driving loop)
```sh
fnr scan --duration 2 -- python3 server.py
fnr port scoring.score_candidates
```

## Real-world HTTP server pattern (with `--load` traffic driver)
For your own FastAPI/uvicorn/http.server app that waits for incoming requests, pass `--load`:
```sh
fnr scan --duration 4 --load "curl -s http://127.0.0.1:8000/score" -- uvicorn main:app
```
`--duration` and `--load` are remembered automatically for `fnr port` and `fnr bench`.

## Try finite benchmark mode
```sh
fnr scan -- python3 server.py --bench
```
