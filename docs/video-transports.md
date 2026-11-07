# Video transports

This document records which live-video transports the pinned upstream sources offer besides MJPEG, how each is negotiated, what a Rust implementation would need, which signals reveal that a device offers it, what this repository does today, and a design for choosing a transport from discovered capabilities. It is an investigation, not an implementation: the adapter implements MJPEG only.

The sources read are PiKVM `kvmd` revision `fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef` (P), GLKVM `glkvm` revision `3e8dd23c4bd638a4650433664cc3b0f3b4d29395` (G) and uStreamer revision `11fd828ab00c01a39bd0843ef442ef0b6d3cac41` (U), the same revisions [compatibility](../COMPATIBILITY.md) cites. Every statement about upstream behaviour links the file and lines it comes from. Statements marked **unverified** are inferences the cited code does not settle, and nothing here was checked against a physical device or a firmware release.

## Summary

| Transport | Offered by | Negotiation | Status in this repository |
| --- | --- | --- | --- |
| MJPEG over HTTP (`/streamer/stream`) with a `/api/ws?stream=true` lease | P, G, U | HTTP GET returning `multipart/x-mixed-replace` | Implemented |
| H.264 over the KVMD media WebSocket (`/api/media/ws`) | P; G ships the server code but no route or service for it | JSON events and binary frames on one WebSocket | Not implemented |
| H.264 (and optional Opus audio) over WebRTC through Janus (`/janus/ws`) | P; G as an optional extra | Janus JSON API on a WebSocket, uStreamer plugin, server-sent SDP offer | Not implemented |
| GLKVM adaptive WebRTC (`webrtc_client`) | G | Not in the inspected source | Not implemented; cannot be designed from the inspected source |
| VNC (RFB) with Tight JPEG or H.264 encodings | P (`kvmd-vnc`), G as an optional extra | RFB on its own TCP port | Not implemented; outside the HTTPS endpoint this adapter configures |

## Shared prerequisites

All the KVMD transports read frames that uStreamer produces, and KVMD runs uStreamer only while a stream client exists. P's main server counts every `/api/ws` session whose `stream` query parameter is true, which is the default ([P server.py L249-L250][p-ws-handler]), and its stream controller starts the streamer when that count, a snapshot in progress or the stream-forever setting needs it ([P server.py L331-L351][p-stream-controller]). A client of any transport therefore holds the same `/api/ws?stream=true` lease the MJPEG implementation holds ([`video::open`](../crates/kvm-kvmd/src/video.rs)) for as long as it reads video. G's stream controller has the same rule except that it also stays stopped in adaptive mode ([G server.py L679-L686][g-stream-controller]).

Over HTTP uStreamer serves only JPEG: its routes are `/`, `/favicon.ico`, `/state`, `/snapshot` and `/stream` ([U server.c L190-L195][u-routes]). Its H.264 output goes to a shared-memory sink (`--h264-sink`, configured in [P v2-hdmi-rpi4.yaml L41-L44][p-main-sinks]), which local services read: the media server, the Janus plugin and the VNC server. A network client reaches H.264 through one of those services.

Both distributions put the KVMD routes behind nginx. P applies `auth_request /auth_check`, which asks KVMD's `/auth/check`, to every location that does not turn it off, including `/api/media/ws`, `/api/media`, `/janus/ws` and `/streamer` ([P kvmd.ctx-server.conf L7-L15, L58-L104][p-nginx]). nginx passes the request's headers to that check, so whichever credentials KVMD's API accepts from the adapter (its session cookie, the `X-KVMD-User` and `X-KVMD-Passwd` headers or Basic credentials) also authenticate these routes. G's base configuration routes `/api/ws`, everything else under `/api` (to the main KVMD server) and `/streamer` ([G kvmd.ctx-server.conf L53-L60, L110-L125][g-nginx]); Janus adds `/janus/ws` through its extra ([G extras/janus/nginx.ctx-server.conf L1-L7][g-janus-nginx]).

## Capability signals

These signals are readable with GET requests and change nothing on the device.

- `GET /api/streamer` returns `result.features.h264`, true when the streamer is configured with an H.264 bitrate ([P streamer/\_\_init\_\_.py L152-L160][p-streamer-state], [P params.py L63, L97-L102][p-params]; [G streamer.py L90, L116-L125][g-features]). G also reports `features.h265`, which is the same value as `h264` in the inspected source. This is the signal both web clients use before offering an H.264 mode ([P stream.js L262-L307][p-stream-js], [G stream.js L191-L242][g-stream-js]). It says the encoder is configured, not that a network transport for it is routed.
- The same response's `result.streamer` is uStreamer's own `/state`, present while the streamer runs, and it contains `h264` (bitrate, GOP, `online`, frame rate) and `sinks.h264` when the H.264 sink is configured ([U server.c L474-L500][u-state]).
- `GET /api/media` on P returns the media server's formats as `result.media.video`, keyed by format, with `h264` carrying `profile_level_id` (`42E01F`) and `jpeg` present only when a JPEG sink is configured for the media server ([P media/server.py L94-L117, L158-L164][p-media-info]). The inspected main configuration for the v2 HDMI Raspberry Pi 4 platform enables only the H.264 sink for it ([P v2-hdmi-rpi4.yaml L47-L50][p-main-media]). A 404 from this route means the media transport is not routed.
- Janus has no KVMD status field. The web clients test for it by importing `/share/js/kvm/janus.js`, which nginx serves from the Janus installation ([P stream\_janus.js L748-L765][p-ensure-janus], [P kvmd.ctx-server.conf L64-L67][p-nginx-janusjs]). A native client can instead open `/janus/ws`, attach the plugin and send its `features` request (below), which reports audio, microphone, camera and ICE settings ([U plugin.c L875-L903][u-features]).
- GLKVM adaptive mode exposes no read-only signal in the inspected source: KVMD keeps it in an in-memory flag ([G server.py L291][g-adaptive-flag]) and nothing in the inspected API reports it (**unverified** for firmware with other code).

## Transports

### MJPEG (implemented)

The adapter opens the `/api/ws?stream=true` lease, then requests `/streamer/stream` and accepts it only as `multipart/x-mixed-replace` with a valid boundary, validating every JPEG part before delivery ([`video::open`](../crates/kvm-kvmd/src/video.rs)). The routes come from the web clients' MJPEG streamers and uStreamer's HTTP server cited in the [adapter's module documentation](../crates/kvm-kvmd/src/lib.rs). It never changes encoder settings.

### H.264 over the media WebSocket

Negotiation on P ([P media/server.py][p-media]):

1. Hold the `/api/ws?stream=true` lease, then open a WebSocket to `/api/media/ws` with the session's credentials ([P kvmd.ctx-server.conf L90-L94][p-nginx-media], [P stream\_media.js L101][p-media-js-ws]).
2. The server sends a JSON text message `{"event_type": "media", "event": {"video": {"h264": {"profile_level_id": "42E01F"}}}}`, listing the formats it has ([P media/server.py L121-L133][p-media-ws]). Text messages are `{"event_type": ..., "event": {...}}` objects ([P htserver.py L234-L266][p-htserver-events]).
3. The client sends `{"event_type": "start", "event": {"type": "video", "format": "h264"}}` ([P stream\_media.js L199-L203][p-media-js-start], [P media/server.py L149-L156][p-media-start]).
4. Each frame arrives as a binary message: byte 0 is the opcode `1`, byte 1 is `1` for a key frame and `0` otherwise, and the rest is the encoded frame ([P media/server.py L205-L221][p-media-sender], [P htserver.py L246-L253][p-htserver-bin], [P stream\_media.js L206-L215][p-media-js-bin]). After a client connects, frames are withheld until the next key frame ([P media/server.py L205-L213][p-media-sender]).
5. The client sends the single byte `0` as a ping, answered with the single byte `255`; the web client pings every second and closes the connection after five unanswered pings ([P media/server.py L135-L138][p-media-ping], [P stream\_media.js L124-L134][p-media-js-ping]). The single byte `1` asks the server for a key frame ([P media/server.py L140-L147][p-media-key]); the inspected web client never sends it and instead waits for the next key frame after recreating its decoder ([P stream\_media.js L218-L242][p-media-js-key]). A client whose queue overflows has its queue cleared and a key frame is requested for it ([P media/server.py L72-L86, L236-L239][p-media-queue]).

P also offers a scripting form: `/api/media/ws?video=h264` starts the stream at once and sends each frame as a raw binary message with no opcode, key flag or JSON ([P media/server.py L121-L133, L216-L217][p-media-ws]). Pings get no reply in this form. The first frame sent is a key frame, but later frames are not marked, so a client must find key frames in the bitstream.

The payload format is **unverified** in the cited sources. The web client configures WebCodecs with the codec string `avc1.42E01F` and no `description` ([P stream\_media.js L199, L239][p-media-js-configure]), and the WebCodecs AVC registration says a bitstream configured without a description is Annex B, with parameter sets in each key chunk ([W3C AVC WebCodecs Registration, section 3][webcodecs-avc], [archived][webcodecs-avc-archive]). The payload is therefore Annex B H.264, constrained baseline profile, with SPS and PPS in key frames, by inference from a working client rather than from the encoder.

On G the media server exists with the same message shapes, though without the root `GET` route or the scripting form, and ignores a second `start` on one connection ([G media/server.py L90-L128, L168][g-media]). The inspected revision has no `kvmd-media` service unit and its nginx sends `/api/media/ws` to the main KVMD server ([G kvmd.ctx-server.conf L110-L116][g-nginx-api]), none of whose API modules registers a `/media` route. The media transport is therefore not reachable on G as inspected (**unverified** on firmware, which may add the route).

A Rust implementation needs: the WebSocket client the adapter already uses for the lease (`reqwest-websocket`), with message and frame size limits sized for an H.264 key frame instead of the lease's header-sized limit; parsing of the JSON events and the two-byte binary header; the ping and key-frame requests; the same lease and cancellation discipline as MJPEG; and a frame type that can carry H.264 access units with a key flag, since `kvm_core::VideoStream` delivers JPEG. A consumer that wants pixels also needs an H.264 decoder. No decoder crate has been evaluated for this repository, and choosing one carries licensing and platform consequences to settle then.

### H.264 over WebRTC through Janus

Negotiation on P, using the Janus JSON API and uStreamer's Janus plugin:

1. Hold the `/api/ws?stream=true` lease, then open a WebSocket to `/janus/ws` ([P kvmd.ctx-server.conf L58-L62][p-nginx-janus], [P stream\_janus.js L278][p-janus-server]). The Janus documentation requires the `janus-protocol` WebSocket subprotocol, `create` and `attach` requests carrying a client transaction string, and periodic `keepalive` requests on a WebSocket session ([Janus RESTful, WebSockets and other API][janus-api], [archived][janus-api-archive]). The Janus gateway itself is not pinned in these sources, so those details are from its current documentation (**unverified** against the Janus build a device ships).
2. Attach the plugin `janus.plugin.ustreamer` ([P stream\_janus.js L377][p-janus-attach]).
3. Send the plugin message `{"request": "features"}` ([P stream\_janus.js L384][p-janus-features]). The plugin answers with `result.status` `"features"` and `result.features` holding `audio`, `mic`, `camera` (`enabled` and an optional `request`) and `ice.url`, a STUN URL that defaults to `stun:stun.l.google.com:19302` unless `JANUS_USTREAMER_WEB_ICE_URL` is set ([U plugin.c L60, L875-L903][u-features]).
4. Send `{"request": "watch", "params": {"orientation": 0, "audio": false, "mic": false, "camera": false}}` ([P stream\_janus.js L704-L717][p-janus-watch]). The plugin replies with status `"started"` and a JSEP **offer** it generated ([U plugin.c L809-L873][u-watch]); the server offers and the client answers. The offer's video is `H264/90000` with `profile-level-id=42E01F;packetization-mode=1`, and audio, when requested, is Opus ([U sdp.c L42-L48, L67-L89][u-sdp]).
5. Create an SDP answer and send it as `{"request": "start"}` with the answer as `jsep` ([P stream\_janus.js L500-L515, L719-L724][p-janus-start], [U plugin.c L803-L804][u-start]). Media then flows over ICE, DTLS and SRTP, not over the WebSocket.
6. `{"request": "key_required"}` asks for a key frame and `{"request": "stop"}` ends the stream ([U plugin.c L805-L807, L905-L907][u-keyreq], [P stream\_janus.js L726-L731][p-janus-keyreq]).

P's plugin configuration reads the H.264 sink and can also capture HDMI audio, play microphone audio to the host and accept a camera stream ([P janus.plugin.ustreamer.jcfg][p-janus-cfg]). KVMD's Janus runner probes a STUN server and starts Janus on the source address the probe found, passing the STUN server on its command line ([P janus/runner.py][p-janus-runner], [P \_scheme.py L790-L815][p-janus-scheme]). G ships the same plugin protocol in its client ([G stream\_janus.js L136, L245, L449, L461][g-janus-js]), with video and audio sinks in its plugin configuration ([G janus.plugin.ustreamer.jcfg][g-janus-cfg]), as an optional extra ([G extras/janus/manifest.yaml][g-janus-manifest]). G's KVMD also serves TURN settings (`username`, `password`, `ttl` and `uris`) from a file at `GET /api/turn/get_turn` ([G api/turn.py L41-L48, L179-L190][g-turn]); the inspected Janus client takes its ICE server only from the plugin's `features` reply ([G stream\_janus.js L150-L157][g-janus-ice]), and which client reads the TURN settings is not in the inspected source (**unverified**).

A Rust implementation needs: a Janus API client (WebSocket with the subprotocol, transactions, events and keepalives); a WebRTC stack providing ICE with STUN, DTLS-SRTP, RTP depacketisation of H.264 in packetization mode 1 and RTCP feedback for picture loss, plus Opus if audio is wanted; handling of trickle ICE; and the same H.264 frame type and decoder choice as the media transport. The plugin's default STUN server is a third-party Google host, so a client must decide whether to use it, the device's configured URL or host candidates only; reaching the device across NAT may need TURN, which P's inspected sources do not configure and G serves only as settings no inspected client reads (**unverified** for deployments). This is the largest of the transports to implement and to test, since a synthetic Janus and a WebRTC peer are needed.

### GLKVM adaptive WebRTC

G's KVMD can run an external `/usr/bin/webrtc_client` in place of uStreamer and Janus ([G server.py L294][g-webrtc-cmd]). `POST /api/streamer/set_params?gl_webrtc=1` records the request ([G server.py L507-L514][g-set-params]), and the stream controller then enters adaptive mode: it stops uStreamer, kills Janus and starts the client under a watchdog, and `gl_webrtc=0` reverses this ([G server.py L361-L389, L679-L712][g-adaptive]). The streamer state reader keeps querying uStreamer's socket in this mode because, per its comment, `webrtc_client` may provide it ([G streamer.py L405-L408][g-compat-socket]). The client binary and its signalling protocol are not in the inspected repository, and the inspected web stream clients do not send `gl_webrtc`.

No client can be designed from the inspected source, and entering the mode is a device-wide side effect: it stops the MJPEG and Janus paths for every viewer. The library must never send `gl_webrtc`. Whether `/streamer/stream` still answers in adaptive mode is **unverified**; if uStreamer's socket is absent, nginx cannot reach its upstream and the adapter sees an HTTP error.

### VNC

P's VNC server reads both the JPEG and H.264 sinks ([P v2-hdmi-rpi4.yaml L53-L58][p-main-vnc]) and implements the Tight JPEG, JPEG and Open H.264 RFB encodings ([P vnc/rfb/encodings.py L37-L39][p-vnc-enc]) on its own TCP port, 5900 by default ([P \_scheme.py L721-L729][p-vnc-scheme]). It is a separate protocol and endpoint from the HTTPS origin `Config::endpoint` names, with its own authentication, so it is recorded here for completeness and not analysed further.

## What the library does when only an unsupported transport is offered

`Device::video` on `Kvmd` calls [`video::open`](../crates/kvm-kvmd/src/video.rs), which tries MJPEG and nothing else:

- If the lease WebSocket is refused, the error comes from `Kvmd::check`: `Error::Authentication` for 401 or 403 and `Error::Http` for other unsuccessful statuses; a successful status that is not an upgrade is `Error::InvalidResponse` ([`video::open`](../crates/kvm-kvmd/src/video.rs), [`Kvmd::check`](../crates/kvm-kvmd/src/lib.rs)).
- If `/streamer/stream` is not routed (a proxy exposing only the media or Janus routes, or a device where uStreamer is stopped), the unsuccessful status is returned the same way, for example `Error::Http(404)` or a proxy's `Error::Http(502)`.
- If the route answers with anything other than `multipart/x-mixed-replace`, `video` returns `Error::Unsupported("device does not expose MJPEG")` ([`video::open`](../crates/kvm-kvmd/src/video.rs)).

In each case no other transport is attempted, no encoder setting is changed, and `capabilities()` keeps reporting `live_video` as `Unknown`, because only a received frame proves it ([`Kvmd::capabilities`](../crates/kvm-kvmd/src/lib.rs)). A caller cannot currently learn from the adapter that H.264 is offered except by reading `features.h264` from the streamer status that `status()` returns, which shows the encoder is configured but not which transport carries it.

## Design for transport selection

This is a proposal for when a second transport is implemented; nothing below exists yet.

1. **Name the transports.** Add a `VideoTransport` enum to `kvm-core` (`Mjpeg`, `MediaH264`, `JanusWebRtc`) and a frame type that is either a JPEG or an H.264 access unit with its key flag. Keep `Device::video` as MJPEG with its current behaviour and add a separate provided method that takes the caller's request, so existing implementations and callers keep compiling. Changing `VideoStream` or adding a field to `Capabilities` instead would break them, and would be released as a breaking change.
2. **Discover without side effects.** Report each transport as a `Capability` from read-only signals only: `MediaH264` is `Supported` when `GET /api/media` lists `video.h264`, `Unsupported` on a 404 from that route, and `Unknown` otherwise; `JanusWebRtc` is `Unknown` until a `features` request through `/janus/ws` succeeds, which a caller triggers only by asking for WebRTC; `features.h264` on its own raises neither transport above `Unknown`, since it does not show that a route exists; `Mjpeg` keeps today's rule, `Unknown` until a frame arrives. Discovery never calls `set_params` and never sends `gl_webrtc`.
3. **Let the caller state what it can consume.** The request carries an ordered list of acceptable transports, since only the caller knows whether it can decode H.264 or run WebRTC. With no list, the adapter uses MJPEG alone, as today.
4. **Select in order, and fall through only on proof of absence.** Take the first listed transport that is `Supported`, otherwise the first that is `Unknown`, and open it. Move to the next transport only when opening fails with `Error::Unsupported` or `Error::Http(404)`; return `Error::Authentication`, `Error::Transport` and other errors at once, because they say nothing about which transports exist and retrying across transports would hide them. A transport that has delivered a frame becomes `Supported` for that adapter instance.
5. **Fail with the whole picture.** When no listed transport opens, return `Error::Unsupported` whose message names each transport tried and why it was rejected, and the transports the device offered that the caller did not list.
6. **Keep the lease and bounds uniform.** Every transport holds the `/api/ws?stream=true` lease for its lifetime, bounds message, frame and buffered sizes before allocating, validates what it can (key-frame flags and Annex B framing for H.264) and releases everything when the stream is dropped, as MJPEG does.
7. **Test against synthetic servers first.** A synthetic media WebSocket server covers the media transport the way the existing synthetic HTTPS device covers MJPEG; WebRTC needs a synthetic Janus and peer. Physical-device results belong in the compatibility matrix, which tracks them separately.

[p-ws-handler]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/apps/kvmd/server.py#L249-L250
[p-stream-controller]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/apps/kvmd/server.py#L331-L351
[p-main-sinks]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/configs/kvmd/main/v2-hdmi-rpi4.yaml#L41-L44
[p-main-media]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/configs/kvmd/main/v2-hdmi-rpi4.yaml#L47-L50
[p-main-vnc]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/configs/kvmd/main/v2-hdmi-rpi4.yaml#L53-L58
[p-nginx]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/configs/nginx/kvmd.ctx-server.conf#L7-L104
[p-nginx-janus]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/configs/nginx/kvmd.ctx-server.conf#L58-L62
[p-nginx-janusjs]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/configs/nginx/kvmd.ctx-server.conf#L64-L67
[p-nginx-media]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/configs/nginx/kvmd.ctx-server.conf#L90-L94
[p-streamer-state]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/apps/kvmd/streamer/__init__.py#L152-L160
[p-params]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/apps/kvmd/streamer/params.py#L63-L102
[p-stream-js]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/web/share/js/kvm/stream.js#L262-L307
[p-media]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/apps/media/server.py
[p-media-info]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/apps/media/server.py#L94-L164
[p-media-ws]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/apps/media/server.py#L121-L133
[p-media-ping]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/apps/media/server.py#L135-L138
[p-media-key]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/apps/media/server.py#L140-L147
[p-media-start]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/apps/media/server.py#L149-L156
[p-media-sender]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/apps/media/server.py#L205-L221
[p-media-queue]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/apps/media/server.py#L72-L239
[p-htserver-events]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/htserver.py#L234-L266
[p-htserver-bin]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/htserver.py#L246-L253
[p-media-js-ws]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/web/share/js/kvm/stream_media.js#L101
[p-media-js-ping]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/web/share/js/kvm/stream_media.js#L124-L134
[p-media-js-start]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/web/share/js/kvm/stream_media.js#L199-L203
[p-media-js-bin]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/web/share/js/kvm/stream_media.js#L206-L215
[p-media-js-key]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/web/share/js/kvm/stream_media.js#L218-L242
[p-media-js-configure]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/web/share/js/kvm/stream_media.js#L199-L239
[p-ensure-janus]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/web/share/js/kvm/stream_janus.js#L748-L765
[p-janus-server]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/web/share/js/kvm/stream_janus.js#L278
[p-janus-attach]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/web/share/js/kvm/stream_janus.js#L377
[p-janus-features]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/web/share/js/kvm/stream_janus.js#L384
[p-janus-start]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/web/share/js/kvm/stream_janus.js#L500-L724
[p-janus-watch]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/web/share/js/kvm/stream_janus.js#L704-L717
[p-janus-keyreq]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/web/share/js/kvm/stream_janus.js#L726-L731
[p-janus-cfg]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/configs/janus/janus.plugin.ustreamer.jcfg
[p-janus-runner]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/apps/janus/runner.py
[p-janus-scheme]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/apps/_scheme.py#L790-L815
[p-vnc-enc]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/apps/vnc/rfb/encodings.py#L37-L39
[p-vnc-scheme]: https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/apps/_scheme.py#L721-L729
[g-stream-controller]: https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/kvmd/apps/kvmd/server.py#L679-L686
[g-nginx]: https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/configs/nginx/kvmd.ctx-server.conf#L53-L125
[g-nginx-api]: https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/configs/nginx/kvmd.ctx-server.conf#L110-L116
[g-janus-nginx]: https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/extras/janus/nginx.ctx-server.conf#L1-L7
[g-features]: https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/kvmd/apps/kvmd/streamer.py#L90-L125
[g-stream-js]: https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/web/share/js/kvm/stream.js#L191-L242
[g-adaptive-flag]: https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/kvmd/apps/kvmd/server.py#L291
[g-media]: https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/kvmd/apps/media/server.py#L90-L168
[g-janus-js]: https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/web/share/js/kvm/stream_janus.js#L136-L461
[g-janus-cfg]: https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/configs/janus/janus.plugin.ustreamer.jcfg
[g-janus-manifest]: https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/extras/janus/manifest.yaml
[g-turn]: https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/kvmd/apps/kvmd/api/turn.py#L41-L190
[g-janus-ice]: https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/web/share/js/kvm/stream_janus.js#L150-L157
[g-webrtc-cmd]: https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/kvmd/apps/kvmd/server.py#L294
[g-set-params]: https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/kvmd/apps/kvmd/server.py#L507-L514
[g-adaptive]: https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/kvmd/apps/kvmd/server.py#L361-L712
[g-compat-socket]: https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/kvmd/apps/kvmd/streamer.py#L405-L408
[u-routes]: https://github.com/pikvm/ustreamer/blob/11fd828ab00c01a39bd0843ef442ef0b6d3cac41/src/ustreamer/http/server.c#L190-L195
[u-state]: https://github.com/pikvm/ustreamer/blob/11fd828ab00c01a39bd0843ef442ef0b6d3cac41/src/ustreamer/http/server.c#L474-L500
[u-start]: https://github.com/pikvm/ustreamer/blob/11fd828ab00c01a39bd0843ef442ef0b6d3cac41/janus/src/plugin.c#L803-L804
[u-keyreq]: https://github.com/pikvm/ustreamer/blob/11fd828ab00c01a39bd0843ef442ef0b6d3cac41/janus/src/plugin.c#L805-L907
[u-watch]: https://github.com/pikvm/ustreamer/blob/11fd828ab00c01a39bd0843ef442ef0b6d3cac41/janus/src/plugin.c#L809-L873
[u-features]: https://github.com/pikvm/ustreamer/blob/11fd828ab00c01a39bd0843ef442ef0b6d3cac41/janus/src/plugin.c#L60-L903
[u-sdp]: https://github.com/pikvm/ustreamer/blob/11fd828ab00c01a39bd0843ef442ef0b6d3cac41/janus/src/sdp.c#L42-L89
[janus-api]: https://janus.conf.meetecho.com/docs/rest.html
[janus-api-archive]: https://web.archive.org/web/20261004163340/https://janus.conf.meetecho.com/docs/rest.html
[webcodecs-avc]: https://www.w3.org/TR/webcodecs-avc-codec-registration/
[webcodecs-avc-archive]: https://web.archive.org/web/20261010022036/https://www.w3.org/TR/webcodecs-avc-codec-registration/
