#### Warning: undocumented APIs do not have any stability guarantees and may change without warning.

-   [MQTT API](#mqtt-api)
-   [HTTP API](#http-api)
    -   [Account](#Account)
    -   [Monitor](#monitor)

<br>

# MQTT API

Enable the `mqtt` plugin and configure the external broker address in `sentryshot.toml`

### sentryshot/events/tflite

``` json
{
  "monitorID": "one",
  "monitorName": "camera_1",
  "label": "person",
  "score": 63.671875,
  "time": "2024-11-20T14:23:15.437494909Z",
  "source": "tflite"
}
```

### sentryshot/events/motion

``` json
{
  "monitorID": "one",
  "monitorName": "camera_1",
  "label": "zone0",
  "score": 48.39815,
  "time": "2024-11-20T14:23:15.437494909Z",
  "source": "motion"
}
```

<br>
<br>

# HTTP API

There is a `/api` page where you can try the endpoints.

All requests require basic auth, POST, PUT and DELETE requests need to have a matching CSRF-token in the `X-CSRF-TOKEN` header.

##### curl examples:

``` shell
curl -u user:pass -X GET https://127.0.0.1/api/accounts

TOKEN=$(curl -k -u user:pass -X GET https://127.0.0.1:2020/api/account/my-token)
#printf "token: %s\n" "$TOKEN"
curl -k -u user:pass -X DELETE https://127.0.0.1:2020/api/account?id=x -H "X-CSRF-TOKEN: $TOKEN"
```
<br>

## Account

### GET /api/account/my-token

##### Auth: user

Returns the temporary CSRF-token for your account.

<br>
<br>

## Monitor


### PATCH /api/monitor/<MONITOR_ID>/motion/enable
### PATCH /api/monitor/<MONITOR_ID>/motion/disable
### PATCH /api/monitor/<MONITOR_ID>/object-detection/enable
### PATCH /api/monitor/<MONITOR_ID>/object-detection/disable


##### Auth: admin

Toggle detector and restart monitor.


### POST /api/monitor/ptz?id=<MONITOR_ID>

##### Auth: admin + CSRF

Send one ONVIF PTZ command to the monitor's camera. The camera
credentials are taken from the stored monitor `mainStream` URL
userinfo — never the request body. The monitor must have `ptz.enable`
saved `true` (set it in the monitor settings form).

Request body:

```json
{
	"command": "move",
	"pan": 0.5,
	"tilt": 0,
	"zoom": 0,
	"timeout": "PT2S",
	"presetToken": "1",
	"presetName": "Gate"
}
```

- `command`: `move` | `stop` | `home` | `getPresets` | `gotoPreset` | `setPreset` | `removePreset` | `capabilities`
- `move`: at least one of `pan`/`tilt`/`zoom` required, each −1.0..=1.0. Optional `timeout` — `PT0.5S` | `PT1S` | `PT2S`, default `PT2S` — is interpolated into the `ContinuousMove` camera-side `Timeout`, bounding how long the camera holds movement without a `stop`. Short taps should send `PT0.5S`; note some firmware ignores `Timeout` and jogs a fixed duration instead (velocity is the only knob there). `timeout` is rejected on any non-`move` command.
- `gotoPreset`/`removePreset`: `presetToken` required.
- `setPreset`: `presetName` required, 1..=64 characters.
- `stop`: stops all axes; `home`: moves to the camera home position.
- `capabilities`: probes the camera and returns the unsupported-command list. Any op the camera faulted `not implemented`/`not recognized` is remembered in the monitor's `ptz.unsupported` and answered `502` on future calls without a camera round trip; the set clears when `ptz.onvifAddr` or `mainStream` changes.

Response `200`:

```json
{
	"presets": [{ "token": "1", "name": "Gate" }]
}
```

```json
{
	"unsupported": ["home", "getPresets"]
}
```

`presets` is present for `getPresets` only, `unsupported` for `capabilities` only; other commands return `{}`.

| Status | Meaning |
| --- | --- |
| `400` | unknown command, missing required field, out-of-range velocity, `presetName` empty or over 64 chars, `timeout` outside the allowlist or attached to a non-`move` command |
| `404` | monitor does not exist |
| `409` | monitor `ptz.enable` is not `true` |
| `502` | camera fault or timeout, or a command the camera marked unsupported |


### POST /api/monitor/audio-session?id=<MONITOR_ID>

##### Auth: admin + CSRF

Mint the single-use WebSocket token for the monitor's two-way audio
channel. No camera contact happens here — the audio session opens
lazily on the first direction ctrl sent over the socket. The monitor
must have `audio.enable` saved `true`.

Response `200` carries `Cache-Control: no-store`:

```json
{ "token": "e01ec7944e1413ba48e41269c6bc30d0" }
```

- The token is ≥128-bit random, bound to the monitor id, single-use,
  and expires 30 s after mint.
- `404` monitor does not exist; `409` `audio.enable` is not `true`,
  or both directions are marked in `audio.unsupported`.
- Camera faults never surface here — they arrive as `error` ctrls on
  the socket.


### GET /api/monitor/audio?token=<TOKEN>

##### Auth: token (see below)

WebSocket upgrade carrying the two-way audio channel. A browser
`WebSocket` can't set the `X-CSRF-TOKEN` header, so the handshake
authenticates on the consumed single-use token and the `Origin`
header must match the request `Host` (host:port, port-normalized) —
a mismatch or missing `Origin` gets `403` *before* the token is
consumed; an unknown or expired token gets `401`.

Text frames carry JSON ctrls. Client→server direction verbs:

```json
{ "type": "listen" | "listen-stop" | "talk-start" | "talk-end" }
```

Server→client messages:

```json
{ "type": "caps", "listen": true, "talk": true, "talkFree": false, "unsupported": ["talk"] }
{ "type": "busy", "command": "talk-start" }
{ "type": "error", "message": "talk unavailable in session" }
```

- `caps` is sent on attach and on grant, and broadcast to every
  attached consumer on talk-lock release and mid-session mark
  changes — `talkFree` is false while another consumer holds the
  talk lock; a `busy`-marked client must wait for a `caps` carrying
  `talkFree: true` before re-marking the control usable.
- Binary frames are PCM Int16 little-endian, 8 kHz mono, ≤20 ms per
  frame — server→client is decoded camera audio, client→server is
  mic audio for encoding to the negotiated G.711 codec. Binary frames
  from a connection that does not hold the talk lock are dropped.
- Attach is lazy: upgrade alone subscribes nothing — a socket with no
  direction ctrl for 10 s is closed (`1008`), and the clock re-arms
  each time a consumer's interest empties. At most 8 consumers per
  monitor; excess attach gets `error` + close `1008`.
- Server-initiated closes carry `1008` for policy triggers
  (directionless, excess attach) and `1000` for operational ones
  (open fault, mid-session transport fault, monitor delete/disable,
  key change). An open fault sends `error` then closes `1000`.
- The session is single-flight per monitor: the first direction ctrl
  opens it, the last detach sends `TEARDOWN`. A session opened
  listen-only stays listen-only — talk capability joins at the next
  open. Monitor delete, `audio.enable` flipping false, or an
  endpoint/credential change tears the session down and closes its
  consumers.


### POST /api/scan

##### Auth: admin + CSRF

Discover cameras on the LAN.

Request body:

```json
{
	"host": "192.168.1.64",
	"username": "admin",
	"password": "optional-camera-credentials"
}
```

- `host` absent or empty: WS-Discovery multicast scan (IPv4 only).
- `host` present: single IPv4 literal — directed probe, vendor fast-paths and an RTSP path shortlist against that one address. No hostname, CIDR or range.

Response `200`:

```json
{
	"cameras": [
		{
			"ip": "192.168.1.64",
			"name": "camera-name",
			"streams": [
				{
					"rtspUrl": "rtsp://user:pass@192.168.1.64:554/main",
					"role": "main",
					"profileName": "MainStream",
					"auth": "digest",
					"videoCodec": "h264"
				}
			],
			"ptz": false,
			"backchannel": false,
			"vigiTalk": false,
			"needsAuth": true
		}
	],
	"truncated": false
}
```

- Every returned stream is DESCRIBE-probed once within the shared deadline; `videoCodec` is the SDP video `encoding_name` (`"h264"`, `"h265"`, …), `"none"` when the SDP has no video stream, or `"unknown"` when the probe could not complete (auth challenge, timeout). SentryShot only plays `h264`.
- `backchannel` reports ONVIF-advertised two-way audio. `vigiTalk` reports a successful TP-Link VIGI private talk probe (port `8800`, run only when ONVIF shows no backchannel and request credentials were supplied) — it records a probe outcome, not capability proof; unreachable, refused, or busy reads `false`. Either flag means two-way audio may work.
- One scan at a time: a semantically different concurrent request gets `409`; an identical one joins the running scan.
- `truncated: true` means the 15-second budget cut the scan — results are partial.
- `400` on invalid `host`, `500` on internal failure. Error bodies are static strings; camera credentials are request-scoped and never persisted by the scan itself.


