# Cloud device, pairing and remote control

The OVH control plane is optional. Local Jellyfin discovery, playback, storage,
CEC and keyboard input do not depend on it. When the cloud is disabled,
unreachable or revoked the TV stays usable; only pairing, remote commands and
YouTube search need the server.

The wire contract is [`protocol-v1.md`](protocol-v1.md) with shared examples in
[`protocol-fixtures.json`](protocol-fixtures.json). This client implements
protocol **v1** and rejects any other major version.

## Staging endpoint

Staging runs at `ovh-1.minotaur-gacrux.ts.net` over Tailscale Serve HTTPS. The
client accepts only an `https://` origin for `server_url`; the WebSocket is
derived as `wss://<origin>/v1/ws/device`. HTTP is never accepted for the cloud,
even on loopback, and credentials in an origin are rejected by configuration.

An operator mints the one-use enrollment token on the server:

```sh
tv-server admin enrollment create --label living-room
```

The token is shown once, is valid for ten minutes, and is consumed by one
successful enrollment. No live token is committed to this repository.

## Enrollment

```sh
tv-client enroll --config /etc/tv-client/client.toml --state-dir /var/lib/tv-client
```

The command reads the one-use token from **stdin**, never from argv, the
environment or a URL. Pipe the value in when terminal scrollback is a concern:

```sh
read -r -s token
printf '%s\n' "$token" | \
  tv-client enroll --config /etc/tv-client/client.toml --state-dir /var/lib/tv-client
unset token
```

The client:

1. Loads the strict TOML configuration and requires `server_url`.
2. Reads one line from stdin and validates a printable opaque token.
3. `POST /v1/enroll` with `{token,label}` over HTTPS/rustls.
4. Requires `protocol: 1` in the response.
5. Stores `device_token` as a 0600 credential file (`device-token`) below the
   state directory.
6. Stores `device_id` as the non-secret preference `cloud.device_id`.
7. Prints only the device id and protocol version.

The one-use token is dropped after the request. The long-lived device bearer is
never written to SQLite, a preference, a log line, a URL, an argv value or a
debug representation. `DeviceToken` and `Enrollment` implement redacted
`Debug`.

`GET /v1/device/config` uses `Authorization: Bearer <device token>` and returns
`{protocol,features:{youtube,airplay,webrtc},heartbeat_ms}`. The response is
validated strictly; a missing token, a non-2xx status or an unsupported
protocol is a typed error and does not stop local playback. Server feature
flags are advisory: local configuration and hardware gates still decide whether
a source can start.

## Device WebSocket

`/v1/ws/device` is opened with the bearer in the `Authorization` header. The
token is never placed in the query string, the URL path, an argv or a log
record. A missing or rejected bearer is a typed authentication failure.

Every frame is a v1 envelope:

```json
{"v":1,"type":"...","id":"uuid","payload":{...}}
```

The device sends `hello` with `{device_id,client_version,capabilities}`, then a
state immediately. `capabilities` currently contains `jellyfin` when a local
Jellyfin origin is configured. `youtube` is advertised only once a real
playback path exists; until C010 is merged `play_youtube` is rejected rather
than acknowledged.

`state` is sent after the hello, on every state change, and every five seconds
while the mode is `playing` or `paused`. `seq` starts at 1 on each connection
and increases monotonically within that connection. Titles are omitted unless
`share_titles = true`; item ids are opaque protocol identifiers and are never
media URLs. Jellyfin credentials, tokens and stream URLs are never transmitted.

Heartbeats are a WebSocket ping every 15 seconds. The connection is considered
dead after 45 seconds without an inbound frame; the client closes it and
reconnects. The server's `/v1/device/config` heartbeat value may refine the
timing, within the protocol-safe bounds validated by the client.

Reconnect uses uniform jitter between 1 and 60 seconds. A fresh connection
starts a new `seq`, sends a new `hello`, and does **not** replay commands or
state from the previous connection. The deduplication cache survives
reconnects, so a command delivered twice is still applied at most once.

The pending command queue is bounded at 32 entries. A full queue is
acknowledged as a safe rejection; the socket reader never blocks and memory
does not grow with an unresponsive application path.

### Malformed and unknown messages

- A frame larger than 32 KiB closes with 1009.
- An unsupported major version closes with 1002.
- An unknown message type, unknown field, binary frame or malformed payload
  closes with 1008.
- Frame text is never copied into a log line or an error message.

`hello_ack`, `pair_request` and `pair_status` are parsed strictly.
`pair_request` requires `request_id`, a four-digit `comparison_code` and an
RFC3339 `expires_at`; `pair_status` requires `pending`, `accepted`, `rejected`
or `expired`.

## Pairing

1. The device posts `/v1/pair/init` with `{}` and the bearer.
2. The server returns `{pair_id,code,expires_at}`. The eight-digit `code`
   (leading zeros preserved) is shown on the TV through the existing Slint
   header banner.
3. The remote submits that code within the tailnet within 120 seconds. The
   server pushes `pair_request` with a four-digit `comparison_code` to the
   device socket.
4. The TV shows the existing `pair.slint` confirmation dialog with the
   comparison code. Only a physical keyboard or CEC event can move the choice
   or confirm it. A remote origin is refused before the request id is
   inspected, and there is no wire action that approves pairing.
5. Accept or deny posts `/v1/pair/decision` `{request_id,accept}` with the
   bearer. The decision HTTP call happens only after local provenance and
   expiry checks pass.
6. The server pushes `pair_status`; `accepted`, `rejected` or `expired` clears
   the local pending request.

An expired request cannot be decided; a second request cannot replace a live
prompt. If the device disconnects, the operator may re-run pair init after
reconnecting; no decision is replayed.

## Remote actions

The server pushes `command` with `{command_id,action}`. The action is decoded
with the shared typed [`Action`](../src/model.rs) and a per-kind field
allowlist, so no unknown field, missing field, wrong type, negative position,
out-of-range volume, malformed YouTube id or `pair_accept`-style pseudo-action
can reach the UI.

Before application the client checks mode and local permissions:

- `set_paused` and `seek` require an active playing or paused session.
- `stop`, `navigate` and `play_jellyfin` are refused in standby.
- `play_jellyfin` additionally requires a configured local Jellyfin origin.
- `standby {enabled:false}` is valid only while already in standby.
- `set_volume` is accepted in every mode within 0..=100.
- `play_youtube` is a typed `unsupported_source` rejection until the C010
  playback path is merged. It is never acknowledged as applied early.

Commands are deduplicated by `command_id` for the last 256 ids over ten
minutes. A duplicate returns the previously recorded acknowledgement without
touching the UI or playback path.

The native launcher starts the cloud runtime when `cloud_enabled` is true and
an enrolled device token exists. The runtime publishes the local UI state
(never a URL), applies commands through the view-model and playback command
paths, and requires the local state to reflect the action before sending an
`applied` acknowledgement. Navigation is applied on the Slint event loop;
play, pause, resume, seek and stop wait for the corresponding UI/player state
change. Remote volume and standby are rejected with a typed
`unsupported_action` reason because this native build has no hardware control
path for them; `play_youtube` remains an unsupported source until C010.

An `ack` carries `{command_id,status,reason}`:

- `applied` is sent only after the owning UI/playback adapter reports that the
  action actually took effect.
- `rejected` is sent for policy, permission, expiry, duplicate-before-completion
  or queue-overflow refusals.
- `failed` is sent when the local path attempted the action and reported a
  failure, including a ten-second application timeout.

The application boundary is the `CommandApplier` trait. Production wiring
passes the typed action through the existing local command path and completes
the future only after application; a headless build uses
`RejectingApplier`, which never claims success. Remote commands cannot approve
pairing, invoke a shell, fetch an arbitrary URL or change a server target.

## Native app wiring

- The native UI shows a persistent cloud status line (`Cloud disabled`,
  `Cloud not enrolled`, `Cloud offline; reconnecting`, `Cloud online`,
  `Cloud access revoked`). Missing storage, configuration or enrollment keeps
  the app running offline; the status line explains why pairing is unavailable.
- Settings → **Pair a remote** requests `/v1/pair/init`. The eight-digit code is
  shown in the header. A pushed `pair_request` opens the existing local
  confirmation dialog with the four-digit comparison code.
- Accept or deny is produced only by the local Slint input path and then
  submitted to `/v1/pair/decision`. A remote-origin action can never create
  that UI command. The runtime uses the physical local provenance class when
  submitting; the exact keyboard/CEC distinction is not carried by the UI
  command today.
- Pairing state and cloud status survive screen navigation. A terminal
  `pair_status` clears the code banner; the confirmation dialog closes on the
  local choice and control returns to the screen underneath.
- The cloud thread has its own bounded shutdown signal and is joined after the
  window exits, like the playback and CEC workers. No token is written to
  argv, logs, the status line or the Slint properties.

## Revocation and offline behavior

A 401/403 during the WebSocket handshake is treated as device revocation: the
link emits `Revoked`, stops reconnecting and returns `LinkEnd::Revoked`. The
device token is not retried or rotated automatically; an operator re-enrolls
after revoking the old device on the server.

Disabling `cloud_enabled`, removing `server_url`, a network outage, an expired
pairing or an unreachable server produces a typed error or a reconnect loop.
None of these is fatal: the local library, playback, input and storage paths
continue to run. No cloud state is required to start or play local Jellyfin
content.

## Tests and integration status

`tests/cloud_device.rs` covers protocol fixture parity, strict action parsing,
the bearer header, hello/state ordering, applied and rejected acknowledgements,
ack-after-application ordering, duplicate suppression, invalid mode, malformed
and unsupported frames, heartbeat ping/pong, reconnect, revocation, pairing
push and local decisions, expired pairing, bounded queue overflow, token
redaction and offline behavior against a local mock server and a recording HTTP
transport.

These are contract tests. No test claims a live staging enrollment, pairing or
remote control session. Live validation requires an operator-minted one-use
token and is recorded separately when available.
