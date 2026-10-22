# Emby Bridge

Rust Emby-compatible gateway for direct playback of local video files and `.strm`
links. No Emby/Jellyfin server is used. Caddy fronts the API; workers scan media
mounts, and the gateway merges their catalogs into one server.

See [API compatibility](docs/API_COMPATIBILITY.md) for the implemented
48 route patterns / 61 HTTP operations, supported query parameters and
the explicitly unimplemented features.

Multi-version files, audio/subtitle track metadata, external text subtitles
and local embedded text subtitle extraction are supported. See
[media layout](docs/METADATA.md) and [client scope](docs/CLIENT_SCOPE.md).

## Docker quick start

1. Copy `.env.example` to `.env`, replace both secrets, and create a `media`
   directory next to `compose.yaml`.
2. Put video files or `.strm` files under `media`. A STRM file contains one
   HTTP(S) video URL on its first nonempty line. An adjacent image such as
   `Film.jpg`, or a folder's `poster.jpg`, becomes its poster.
3. Run `docker compose up --build -d`. Add `http://localhost:8096` as an Emby
   server in the client and sign in with `EMBY_ADMIN_NAME` and
   `EMBY_ADMIN_PASSWORD`.

The bundled Caddyfile binds HTTP only to the Docker host's loopback for local
evaluation. For internet/LAN clients, set `EMBY_DOMAIN` to a real domain in
`.env`, point its DNS at this host, and run
`docker compose -f compose.yaml -f compose.production.yaml up --build -d`.
The production override exposes 80/443 and enables Caddy automatic HTTPS.
Do not publish port 8096 over plain HTTP or expose worker ports/the
gateway's internal port. This override uses Docker Compose's `!override`
tag (Compose 2.24.4 or newer) to remove the local-only 8096 binding.

## Nodes

The same binary has `gateway` and `worker` roles (`EMBY_ROLE`). Each worker
registers its full catalog, then heartbeats every 10 seconds. Only changed
catalogs are re-sent; the default scan interval is 60 seconds. An unavailable
node stops receiving requests after the gateway TTL. Logical item IDs derive
from the library name and relative path: workers with the same item form
replicas, and the gateway chooses a healthy replica by weighted least-active
streams. Distinct media on different nodes appear in one library. The two
example workers mount the same `media` directory to demonstrate replicas.
Replicas should contain identical media at the same relative path; known
size/container mismatches are not routed as interchangeable copies. A failed
node is temporarily excluded and the gateway tries another healthy replica.
Registration accepts up to 100,000 items in a bounded 32 MiB request; larger
catalogs need a future paged sync protocol.

To add a worker, give it a distinct `EMBY_NODE_ID` and reachable
`EMBY_ADVERTISE_URL`, the same `EMBY_INTERNAL_KEY`, a
`EMBY_GATEWAY_URL`, and either `EMBY_LIBRARIES_JSON` (an array of
`{"name":"Media","path":"/media"}`) or `EMBY_MEDIA_ROOT`. Use the same
library name on nodes whose contents should merge. `EMBY_NODE_WEIGHT`
controls replica preference. The authenticated `/Bridge/Nodes` endpoint
shows health, item counts, and active streams.

Local files stream from worker through the gateway, preserving byte-range
requests. STRM targets return a 302 to the client; the client must reach the
target itself, and the target must support playback and seeking. Never place a
permanent cloud master credential in a STRM URL. Providers that require extra
headers or return private-network URLs need a future resolver/proxy adapter.
PlaybackInfo grants a media-specific, two-hour playback ticket so external
players need not carry the long-lived Emby token to the stream endpoint.
The Docker image includes `ffprobe`: ordinary files are probed in the
background for duration and tracks, with results cached by size and
modification time. Set `EMBY_FFPROBE` to an empty string to disable probing.
STRM URLs are not probed remotely.

## Local metadata and TV

Movie, series, season and episode NFO sidecars are read locally. `S01E01`
filenames can identify episodes without NFO; flat episodes are grouped into
stable virtual seasons. Both ordinary files and STRM support this hierarchy.
Season/episode browsing and NextUp use the same merged, multi-node catalog.
See [metadata layout and limitations](docs/METADATA.md) for directory examples,
supported fields and the deliberately limited NextUp policy.

## Extension points

`EMBY_HOOK_URLS` is an optional comma-separated list of HTTP endpoints on
either role. Events (`catalog.before_scan`, `catalog.changed`,
`node.before_register`, `node.registered`, `auth.succeeded`, `auth.failed`,
`route.selected`, `route.failed`, `stream.local`, `stream.redirect`,
`playback.progress`, `user_data.changed`, `preferences.updated`,
`user.configuration.updated`) are delivered as JSON with `event`, `role`,
`node_id`, and `data`. Delivery is best-effort with a two-second timeout and
does not block playback. No passwords, tokens, or full STRM URLs are sent.
The catalog, registry/selection, and media relay are separate Rust modules.
`RoutingPolicy` and `SourceResolver` are Rust extension interfaces for
replacing node selection and media source handling without changing the API.

## Current scope

Single user, direct play only; no transcoding, cloud-account login, online
metadata scraping, or full Emby API coverage. The
gateway stores its server ID, login tokens, playback state, favorites and
client preferences in SQLite. Active sessions track client capabilities and
current playback in memory; no remote-control or WebSocket channel exists.
Hills Lite 1.5.5 has been checked with login, browsing, item details, and
direct playback of a short local MP4. Seeking, external subtitles, multiple
versions, a reachable STRM target, and Yamby still need client-side checks.
For STRM playback, the video target's final `206 Content-Range` response
matters more than the initial 302. Unhandled API paths are logged by the
gateway.
