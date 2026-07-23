# PTC ISMS OpenAB Bridge

This integration keeps the existing Teams Bot Worker and adds an optional internal
bridge to OpenAB. The bridge is not a public Teams webhook and should stay on
Jetson localhost.

## Flow

```text
Teams/Azure Bot
    -> existing Cloudflare Worker
    -> existing jetson-isms-worker
    -> POST http://127.0.0.1:8080/webhook/ptc-isms
    -> OpenAB Gateway event
    -> OpenAB Agent
    -> matching JSON response
    -> existing worker reply path
```

The current ISMS and Confluence/Ollama paths remain unchanged. The worker switch
to this bridge is a separate rollout step.

## Build

From the repository root:

```bash
cargo check --features unified
cargo test -p openab-gateway --features ptc-isms
```

The `unified` feature includes `ptc-isms`. The standalone gateway package also
supports `--features ptc-isms`.

## Recommended Unified Mode

Run OpenAB with the existing unified server and keep the listener local:

```bash
export GATEWAY_LISTEN=127.0.0.1:8080
export PTC_ISMS_BRIDGE_SECRET='generate-a-long-random-secret'
export PTC_ISMS_BRIDGE_WEBHOOK_PATH=/webhook/ptc-isms
export PTC_ISMS_BRIDGE_TIMEOUT_SECS=180
cargo run --features unified
```

Setting `PTC_ISMS_BRIDGE_SECRET` enables the route. Without that variable the
route is not registered.

## Worker Request Contract

```http
POST /webhook/ptc-isms
Content-Type: application/json
X-PTC-ISMS-SECRET: <same shared secret>

{
  "requestId": "req-20260723-0001",
  "text": "What is the ISMS approval process?",
  "senderId": "jetson-isms-worker",
  "senderName": "PTC ISMS Worker",
  "channelId": "ptc-isms"
}
```

Successful response:

```json
{
  "requestId": "req-20260723-0001",
  "answer": "..."
}
```

An invalid secret returns HTTP 401. A missing OpenAB WebSocket/event consumer
returns HTTP 503. A timeout returns HTTP 504.

## Standalone Gateway Mode

If OpenAB core and the gateway run as separate processes, configure the generic
gateway adapter in OpenAB:

```toml
[gateway]
url = "ws://127.0.0.1:8080/ws"
platform = "ptc-isms"
token = "${GATEWAY_WS_TOKEN}"
allow_all_channels = true
allow_all_users = false
allowed_users = ["jetson-isms-worker"]
streaming = false
```

Start the gateway with the same bridge secret and keep it local:

```bash
export GATEWAY_LISTEN=127.0.0.1:8080
export GATEWAY_WS_TOKEN='another-long-random-secret'
export PTC_ISMS_BRIDGE_SECRET='the-bridge-secret'
cargo run -p openab-gateway --features ptc-isms
```

## Security Rules

- Bind the listener to `127.0.0.1`, not `0.0.0.0`, unless a firewall rule is added.
- Store `PTC_ISMS_BRIDGE_SECRET` in systemd EnvironmentFile or another secret store.
- Never commit bridge secrets, worker `.env` files, or OpenAB runtime logs.
- Do not expose `/webhook/ptc-isms` through Cloudflare or a public reverse proxy.
- Keep `streaming = false` for the current Teams worker contract.

## Rollout Status

- OpenAB adapter, route registration, reply correlation, and tests: complete.
- Existing `ui_ollama`: unchanged.
- Existing `jetson-isms-worker`: unchanged.
- Worker feature flag and production systemd configuration: next step.
