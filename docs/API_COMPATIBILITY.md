# Emby API compatibility

## Coverage

The gateway currently exposes **48 Emby route patterns / 61 HTTP operations**.
GET and HEAD count separately; `/emby` aliases, `/health`, `/Bridge/Nodes`,
and worker/cluster internal APIs are excluded. This is an inventory of this
implementation, not a percentage of the complete Emby API.

All routes also work with the `/emby` prefix. Unless explicitly public,
requests require a login token. Only user ID `1` exists. No Emby/Jellyfin
server is involved.

| Methods | Path | Behavior |
| --- | --- | --- |
| GET | `/System/Ping` | Public availability response |
| GET | `/System/Info/Public` | Public server ID/name/version/addresses |
| GET | `/System/Info` | Authenticated basic system information |
| GET | `/Users/Public` | Public sign-in identity, no administrative configuration |
| POST | `/Users/AuthenticateByName` | Password login, persistent token, session information |
| GET | `/Users` | The configured single user |
| GET | `/Users/Me` | Current user and saved configuration |
| GET | `/Users/{user}` | User identity/configuration |
| POST | `/Users/{user}/Configuration` | Merge and persist client user preferences |
| GET | `/Users/{user}/Views` | Deduplicated media libraries |
| GET | `/Users/{user}/Items` | Filtered, sorted, paginated items |
| GET | `/Items` | Shared query engine; defaults to recursive selection |
| GET | `/Users/{user}/Items/Root` | Synthetic root folder |
| GET | `/Users/{user}/Items/Latest` | Latest matching videos; array response |
| GET | `/Users/{user}/Items/Resume` | Unplayed, non-hidden videos with progress |
| GET | `/Users/{user}/Items/{id}` | Detail and persisted user data; also accepts root ID |
| GET | `/Items/{id}` | Detail; also accepts root ID |
| GET | `/Items/Counts` | Counts of currently visible catalog entries |
| GET | `/Items/Filters` | Distinct genres, tags and years from matching local metadata |
| GET | `/Shows/{id}/Seasons` | Series seasons ordered by index, with episode counts |
| GET | `/Shows/{id}/Episodes` | Series episodes; SeasonId/Season, StartItemId and paging |
| GET | `/Shows/NextUp` | One next unplayed episode per series; see metadata policy |
| GET | `/Items/{id}/Ancestors` | Immediate parent first, ending at root |
| GET | `/Items/{id}/Similar` | Same-folder video suggestions, not metadata-based recommendations |
| GET | `/Search/Hints` | Case-insensitive name search with pagination |
| GET | `/Library/VirtualFolders` | Logical libraries; no private worker filesystem paths |
| POST, DELETE | `/Users/{user}/FavoriteItems/{id}` | Persist favorite state; return user data |
| POST, DELETE | `/Users/{user}/PlayedItems/{id}` | Persist played state; return user data |
| GET, POST | `/Users/{user}/Items/{id}/UserData` | Read/update progress, played and favorite state |
| POST | `/Users/{user}/Items/{id}/HideFromResume` | Persist `Hide=true/false` |
| GET, POST | `/DisplayPreferences/{id}` | Persist preferences per ID/user/client |
| GET, POST | `/Items/{id}/PlaybackInfo` | Direct-play sources and media-specific playback ticket |
| GET | `/Items/{id}/Images` | Primary image descriptor when a local poster exists |
| GET, HEAD | `/Items/{id}/Images/{image_type}` | Original primary image |
| GET, HEAD | `/Items/{id}/Images/{image_type}/{index}` | Primary image index 0 |
| GET, HEAD | `/Videos/{id}/stream` | Authenticated direct stream or STRM redirect |
| GET, HEAD | `/Videos/{id}/{tail}` | Only `stream.<extension>` tails are accepted |
| GET, HEAD | `/Items/{id}/{source}/Subtitles/{index}/Stream.{format}` | Authenticated/ticketed text subtitle delivery |
| GET, HEAD | `/Videos/{id}/{source}/Subtitles/{index}/Stream.{format}` | Same subtitle delivery, video compatibility alias |
| GET, HEAD | `/Items/{id}/{source}/Subtitles/{index}/{start}/Stream.{format}` | Text subtitle delivery with start ticks; ASS offset unsupported |
| GET, HEAD | `/Videos/{id}/{source}/Subtitles/{index}/{start}/Stream.{format}` | Video alias with start ticks |
| GET | `/Sessions` | Active sessions, optional `DeviceId` filter |
| POST | `/Sessions/Capabilities` | Legacy capability reporting |
| POST | `/Sessions/Capabilities/Full` | JSON capability reporting |
| POST | `/Sessions/Playing` | Record current playback, play count and last-played time |
| POST | `/Sessions/Playing/Progress` | Update progress and session play state |
| POST | `/Sessions/Playing/Stopped` | Clear current playback; mark played at >=90% of known duration |
| POST | `/Sessions/Logout` | Revoke login token and remove session |

## Query support

`ParentId`, `Recursive`, `Ids`, `SearchTerm`, `IncludeItemTypes`,
`ExcludeItemTypes`, `MediaTypes`, `IsFolder`, `IsFavorite`, `IsPlayed`,
`StartIndex` and `Limit` are supported. Query parameter names are
case-insensitive. Search uses Unicode lowercase matching.
`SeriesId`, `SeasonId`, `Genres`, `Tags` and `Years` also filter items.
Genres/tags accept pipe- or comma-separated alternatives; years accept commas.

Supported `Filters`: `IsFavorite`, `IsFavoriteOrLikes`, `IsPlayed`,
`IsUnplayed`, `IsFolder`, `IsNotFolder`, `IsResumable`.

Supported `SortBy`: `SortName`/`Name`, `DateCreated`/`DateAdded`,
`DatePlayed`, `PlayCount`, `Runtime`, `IsFavoriteOrLiked`, `IndexNumber`,
`ParentIndexNumber`, `PremiereDate`, `ProductionYear`. Comma-separated
sort fields and ascending/descending orders are supported. Stable item IDs
break ties. Unknown sort fields fall back to name; other unimplemented
query parameters/filters are not applied.

`DateCreated` currently uses filesystem modification time as a stable
approximation, not a separately tracked library insertion date.
Items can be `Video`, `Movie`, `Series`, `Season`, `Episode`, `Folder` or
`CollectionFolder`. Movie filtering now selects NFO-classified movies only;
`Video` remains an umbrella filter for all playable videos. Counts reflect
the recognized types. Recursive queries follow logical parent links, including
virtual seasons, rather than filesystem path prefixes.
See [local metadata](METADATA.md) for recognition and supported NFO fields.

## State and limitations

SQLite persists login tokens, server identity, progress, favorites,
hidden-resume flags, played state, play counts, last-played time, display
preferences and user configuration. Existing databases are migrated in place.
User configuration changes do not change authorization policy or server
settings. Sessions are in memory, expire after 30 idle minutes and are
recreated when a persisted token is next used.

Local files support a single byte range, HEAD, 206/416 and ETag/If-Range.
PlaybackInfo accepts query or JSON `MediaSourceId`, `AudioStreamIndex` and
`SubtitleStreamIndex` (`-1` disables subtitles). Invalid sources/tracks are
rejected. Audio selection is conveyed to the client, not applied by rewriting
the file or remuxing the redirect target.

Explicit `[version=label]` filenames aggregate into one logical item across
nodes. `MediaSources` contains source-specific tracks, size, duration, container
and stable IDs; `MediaSourceCount` describes the number of versions. Streaming
validates membership before routing the chosen version to a healthy replica.
Replicas must also match stream metadata, excluding private subtitle paths.
Each logical item has shared progress/favorites. See [media layout](METADATA.md)
and [client scope](CLIENT_SCOPE.md).

Workers discover SRT/ASS/SSA/VTT sidecars and retain ffprobe language, title,
default and forced flags for audio/subtitle tracks. Subtitle URLs accept login
tokens or the parent item's playback ticket. Native external SRT/ASS/VTT files
support Range/HEAD; ffmpeg converts text subtitle formats and extracts local
embedded text tracks. These operations do not transcode video/audio. Conversion
is bounded to two concurrent jobs per worker, 15 seconds and 16 MiB output;
busy/unavailable workers return 503. ASS start offsets and bitmap subtitle
extraction return unsupported media type. Embedded STRM subtitles remain
client-decoded; workers never fetch remote sources for extraction.

Original primary images are served without resizing, format conversion or
additional backdrop/index support. STRM currently requires one client-reachable
HTTP(S) URL; no provider-specific login, header injection or URL refresh.
Failed ffprobe attempts are retried on a later scan after a 60-second backoff.

Client playback capability reports are stored, but WebSocket push and remote
playback control are not implemented or advertised as available. Completed
playback detection needs a known runtime from ffprobe or an NFO `runtime`;
STRM URLs themselves are not probed.

## Remaining work

- User audio/subtitle preference persistence, ASS fonts and subtitle extraction caching.
- Online metadata scraping, people, collections and metadata-based recommendations.
- Richer episode naming, multi-episode files, missing/virtual episodes and full Emby NextUp semantics.
- Multiple users, user administration, password changes and per-library permissions.
- Provider-specific STRM resolvers, expiring cloud links and header-aware proxying.
- WebSocket notifications, remote-control sessions and broader client-specific routes.
- Live TV, DVR, playlists, music libraries, plugins and Emby server administration.
- Transcoding remains intentionally out of scope.

Unsupported endpoints return 404 and are logged. This project does not claim
complete Emby compatibility; Hills/Yamby/SenPlayer still require real-device testing.
