# pulse

A super tiny HTTP webserver that answers `200 ok` for Uptime Kuma to ping, so a
VPS stays awake. One file `main.rs`, no per-request logging, no `Server` header,
and it leaks no hostname, version, or client IP.

## Behavior

- `GET` and `HEAD` on the health path (default `/health`) → `200 OK`, body `ok`,
  `Content-Type: text/plain; charset=utf-8` (empty body for HEAD).
- Every other path, including `/`, and every method other than GET/HEAD →
  `404 Not Found`, empty body.
- Global rate limit (shared across all clients, not per IP): requests above
  `RATE_LIMIT_RPS` per second get `429 Too Many Requests`, empty body.
- Request bodies with a `Content-Length` over 1 KB → `413 Payload Too Large`.
- 5 second request timeout, at most 64 requests in flight.
- Graceful shutdown on SIGTERM or SIGINT.

## Configuration (env)

| Env              | Default   | Description                                            |
| ---------------- | --------- | ------------------------------------------------------ |
| `PORT`           | `8080`    | Port to listen on, bound to `0.0.0.0`.                 |
| `HEALTH_PATH`    | `/health` | Health check path. A missing leading `/` is added automatically. |
| `RATE_LIMIT_RPS` | `20`      | Global rate limit in requests per second. `0` or non-numeric values are treated as invalid and fall back to the default. |

## Build

```sh
cargo build --release
```

Binary output: `target/release/pulse`. The `release` profile is optimized for
small size (`opt-level = "z"`, LTO, strip, `codegen-units = 1`,
`panic = "abort"`). Primary target: Linux x86_64.

## Run

```sh
PORT=8080 HEALTH_PATH=/health RATE_LIMIT_RPS=20 ./target/release/pulse
```

A single log line is printed at startup only, for example
`pulse listening on 0.0.0.0:8080`.

## Test

```sh
cargo test
```

## Try it with curl

```sh
curl -i http://127.0.0.1:8080/health
curl -i -X HEAD http://127.0.0.1:8080/health
curl -i http://127.0.0.1:8080/
curl -i http://127.0.0.1:8080/anything-else
curl -i -X POST http://127.0.0.1:8080/health
curl -i -X POST --data-binary @big-file.bin http://127.0.0.1:8080/health
for i in $(seq 30); do curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8080/health; done
```

Expected: `GET /health` returns `200 ok`, `HEAD` returns `200` with no body, `/`
and any other path return `404` with an empty body, methods other than
GET/HEAD return `404`, bodies over 1 KB return `413`, and the thirty-request
loop will show some `429`s from the rate limit.

## Install as a systemd service

```sh
sudo useradd --system --no-create-home --shell /usr/sbin/nologin pulse
sudo install -m 755 target/release/pulse /usr/local/bin/pulse
sudo install -m 644 pulse.service /etc/systemd/system/pulse.service
sudo systemctl daemon-reload
sudo systemctl enable --now pulse
sudo systemctl status pulse
```

The unit runs as a non-root user (`pulse`) with `Restart=always`. The default
env values (`PORT=8080`, `HEALTH_PATH=/health`, `RATE_LIMIT_RPS=20`) are
already set in `pulse.service`; edit them there if you need different values.
