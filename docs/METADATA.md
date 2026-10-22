# Local metadata and TV

No scraper, Emby server or external metadata request is required. Workers
read local NFO files on each scheduled scan and send structured metadata with
their catalog. Updating NFO changes the catalog revision but not media IDs or
saved progress. NFO-derived titles never replace a file's streaming path.

## Versions, Audio and Subtitles

```text
media/
  Film [version=1080p].mkv
  Film [version=4K].strm
  Film [version=4K].nfo
  Film [version=4K].ffprobe.json
  Film [version=4K].zho.default.ass
  Film [version=4K].eng.forced.srt
```

The explicit ` [version=label]` suffix groups files in the same relative
directory/library into one item, even if versions live on different workers.
It also applies to episodes, e.g. `Show.S01E01 [version=4K].mkv`.
Labels must be nonempty and are displayed as MediaSource names. Untagged files
are not automatically merged. The logical ID stays stable when adding versions;
each physical filename retains its independent source ID. Renaming a version
changes its source ID; renaming the base title changes the logical ID.
NFO is per physical filename. Versions should supply consistent movie/episode
metadata. A single marked version already uses the stable logical ID.

Adjacent subtitles must use the complete source stem: `<stem>.srt` or
`<stem>.<language>[.default][.forced].srt`; ASS, SSA and VTT are also recognized.
Sidecar files are limited to 16 MiB and symlinks are ignored. The first label
other than default/forced/sdh/cc is the language; remaining text is retained in
the display title. Stream indexes follow the highest embedded stream index.
Replicas must carry the same media info and sidecars so indexes agree.

Local files are probed with ffprobe. STRM sources are not fetched by the worker;
optional `<stem>.ffprobe.json` supplies cached metadata using the normal
ffprobe `-show_format -show_streams -of json` schema, limited to 1 MiB:

```json
{
  "format": {"duration": "120"},
  "streams": [
    {"index": 0, "codec_type": "video", "codec_name": "hevc", "width": 3840, "height": 2160},
    {"index": 1, "codec_type": "audio", "codec_name": "aac", "tags": {"language": "zho", "title": "Mandarin"}, "disposition": {"default": 1}},
    {"index": 2, "codec_type": "subtitle", "codec_name": "ass", "tags": {"language": "eng"}}
  ]
}
```

Generate this metadata from a trusted source and refresh it when the target
changes. Local paths/URLs in the JSON are not used for serving files.
PlaybackInfo exposes all sources or filters by `MediaSourceId`; optional
`AudioStreamIndex` and `SubtitleStreamIndex` select source-local defaults.
No audio stream is removed from the original video, and there is no remuxing.

External text subtitles can be served in their native format or converted
using ffmpeg; local embedded text subtitles can also be extracted. Only SRT,
ASS and VTT output is accepted. Bitmap subtitle extraction, ASS start offsets
and remote STRM embedded extraction are not supported. Clients must decode
embedded audio/bitmap subtitles themselves. `EMBY_FFMPEG` may override the
ffmpeg executable (the Docker image already includes ffmpeg).

## Layout

```text
media/
  Film/
    Film.mkv
    Film.nfo
    poster.jpg
  Example Show/
    tvshow.nfo
    poster.jpg
    Season 01/
      season.nfo
      Example.S01E01.strm
      Example.S01E01.nfo
      Example.S01E02.mp4
    Example.S02E01.strm
```

The configured media root is a library; place individual shows in subfolders.
Folders with `tvshow.nfo` become Series. A video with an adjacent
`<episodedetails>` NFO or an `S01E01` filename becomes Episode. Season folder
names `Season 01`, `Season01`, `S01` and `Specials` are recognized when they
contain recognized episodes. `season.nfo` can identify other season folders.
An otherwise generic folder containing episodes or seasons becomes Series.
Explicit movie NFO takes precedence over filename-based episode detection.

Flat episodes directly inside a show get stable virtual Season folders,
including season zero. Episodes retain their original IDs. A physical season
with the same series/number is reused when present. Client ancestry and
recursive queries follow this logical hierarchy. Replicas should use the same
layout, filenames and sidecars to advertise matching metadata.

For a video, `<filename>.nfo` is preferred. `movie.nfo` is a fallback only
when the directory contains exactly one candidate video/STRM. A `<movie>`
root classifies a file as Movie; files without metadata remain generic Video.
Pure episode libraries report `CollectionType=tvshows`, pure classified movie
libraries report `movies`, and mixed/generic libraries leave it unset.

## NFO Fields

Supported roots: `movie`, `tvshow`, `season`, `episodedetails`.

- `title`, `originaltitle`, `sorttitle`, `plot` (or `outline`).
- `year`, `premiered` (or `aired`), legacy numeric `rating` and `mpaa`.
- Repeated `genre` and `tag`.
- `uniqueid` with `type=imdb`, `tmdb` or `tvdb`.
- Episode `season` and `episode`; season `seasonnumber`.
- Integer `runtime` in minutes, used only when ffprobe runtime is unavailable.

```xml
<episodedetails>
  <title>Pilot</title>
  <season>1</season>
  <episode>1</episode>
  <aired>2024-01-02</aired>
  <runtime>42</runtime>
  <plot>The opening episode.</plot>
</episodedetails>
```

NFO is limited to 1 MiB and 50,000 XML nodes. DTDs/entities are rejected;
standard XML escapes such as `&amp;` are supported. Sidecar symlinks are not
read. Malformed/unsupported NFO is logged and skipped without aborting the
scan. Remote image URLs, actor lists, XML streamdetails, collections, nested
rating schemas and arbitrary provider fields are not consumed.

## TV Endpoints

All require authentication and accept `UserId=1`.

- `GET /Shows/{id}/Seasons`: seasons ordered numerically by default.
- `GET /Shows/{id}/Episodes`: optional `SeasonId` or numeric `Season`;
  defaults to season/episode number order. `StartItemId` is inclusive.
- `GET /Shows/NextUp`: optional `SeriesId`, `ParentId`, paging and
  `EnableFirstEpisode` (default true).

NextUp returns at most one episode per show, skipping specials and known
future premiere dates. It resumes the highest-numbered started episode if
unplayed, otherwise selects the first unplayed episode after it. Unstarted
shows offer their first episode unless `EnableFirstEpisode=false`. Shows are
ordered by their latest recorded playback activity, then series name.
Watched gaps before the last started episode are not backfilled.

This is a deliberately limited NextUp policy, not the full Emby algorithm.
There are no missing episodes, online schedules, multi-episode file splitting
or arbitrary filename parsers. Unknown episode season defaults to 1 when it
belongs to a show; numbering supplied by NFO takes precedence over filenames.
Poster delivery remains unchanged. Hills/Yamby still need real-client testing.
