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
	"presetToken": "1",
	"presetName": "Gate"
}
```

- `command`: `move` | `stop` | `home` | `getPresets` | `gotoPreset` | `setPreset` | `removePreset`
- `move`: at least one of `pan`/`tilt`/`zoom` required, each −1.0..=1.0. The camera holds movement for ~2s (server-side `Timeout`) unless a `stop` follows.
- `gotoPreset`/`removePreset`: `presetToken` required.
- `setPreset`: `presetName` required, 1..=64 characters.
- `stop`: stops all axes; `home`: moves to the camera home position.

Response `200`:

```json
{
	"presets": [{ "token": "1", "name": "Gate" }]
}
```

`presets` is present for `getPresets` only; other commands return `{}`.

| Status | Meaning |
| --- | --- |
| `400` | unknown command, missing required field, out-of-range velocity, `presetName` empty or over 64 chars |
| `404` | monitor does not exist |
| `409` | monitor `ptz.enable` is not `true` |
| `502` | camera fault or timeout |


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
			"needsAuth": true
		}
	],
	"truncated": false
}
```

- Every returned stream is DESCRIBE-probed once within the shared deadline; `videoCodec` is the SDP video `encoding_name` (`"h264"`, `"h265"`, …), `"none"` when the SDP has no video stream, or `"unknown"` when the probe could not complete (auth challenge, timeout). SentryShot only plays `h264`.
- One scan at a time: a semantically different concurrent request gets `409`; an identical one joins the running scan.
- `truncated: true` means the 15-second budget cut the scan — results are partial.
- `400` on invalid `host`, `500` on internal failure. Error bodies are static strings; camera credentials are request-scoped and never persisted by the scan itself.


