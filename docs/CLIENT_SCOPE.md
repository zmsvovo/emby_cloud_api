# Hills / Yamby / SenPlayer compatibility scope

The target is the direct-play client workflow, not the complete Emby API.
Client versions/platforms and real-device validation remain to be recorded.
No client-specific required-call list has been verified yet.

The Excel checklist currently preselects 75 HTTP operations: 61 have basic
implementations and 14 are proposed enhancements. Existing route limitations
and user notes remain visible; selection does not imply client certification.

## Current Batch

- Login, browsing, searching, shows, resume, favorites and session state:
  existing partial implementations, not real-client certified.
- Audio tracks: codec, language, title, default flags, source-specific indexes
  and PlaybackInfo selection. The client switches tracks in the original file.
- Subtitles: embedded track metadata, adjacent SRT/ASS/SSA/VTT discovery,
  delivery URLs, default/forced flags, subtitle off (`-1`), text conversion
  and local embedded text extraction.
- Versions: explicit filename grouping, stable logical/source IDs, one item
  with multiple MediaSources, source validation, node routing and shared
  logical-item progress. Versions on different nodes are aggregated; replicas
  of the same version use weighted least-active routing.
- Hooks: `playback.prepared`, `route.selected` (includes `source_id`) and
  `subtitle.requested`, in addition to existing scan/register/playback hooks.
- STRM metadata: bounded local `.ffprobe.json`; no scan-time remote probing.

## Remaining Work

1. Validate actual Hills/Yamby/SenPlayer request/response expectations, including
   source selection, subtitle-off, alternate audio and episode continuation.
2. Add selected home-section, genre/tag/year browsing and optional extra-media
   responses when required by the chosen client workflows. The Excel selections
   are a proposed scope, not evidence that each client calls every endpoint.
3. Expand image types beyond Primary, then address image size/format behavior.
4. Add user audio/subtitle preference application and persistence per version.
5. Add pluggable STRM metadata/URL refresh providers and optional controlled
   remote probing; no unbounded network fetches during library scans.
6. Consider ASS attachment/font delivery, subtitle extraction caching and
   better handling of unusually large or legacy-encoded subtitle files.

## Explicit Exclusions

No video/audio transcoding or remuxing, subtitle burn-in, server-side switching
of external audio tracks, live TV/DVR, offline download, admin/plugin APIs or
remote control/WebSocket implementation in this batch. Bitmap subtitles stay
embedded and require client support. STRM embedded subtitle extraction is not
performed on workers.

## Focused Acceptance

Use one ordinary multi-audio file, one real reachable STRM with sidecar media
information, a two-episode show, two versions of a film and external subtitles.
For each selected client version/platform: connect, browse/search, choose a
version, play/seek, switch audio/subtitles, disable subtitles, save/resume and
advance episode. Check auth/source isolation with focused API tests.
Passing route-level tests alone does not establish client compatibility.

## Verification on 2026-10-21

18 Rust tests pass, including source membership/ticket isolation, cross-node
version aggregation, subtitle HEAD, selected audio indexes and STRM metadata.
A focused Docker smoke check passed with two healthy workers: two versions,
two audio tracks in the selected version, Range 206, subtitle-off, external
SRT-to-VTT conversion and embedded text subtitle extraction. Temporary media
was removed after the check. This does not replace real-client validation.
