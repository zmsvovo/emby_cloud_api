use std::collections::HashMap;

use serde_json::{Value, json};

use crate::model::{ItemKind, MediaItem};

pub const COMPAT_VERSION: &str = "4.10.1.0";

pub fn public_info(server_id: &str, name: &str, base_url: &str) -> Value {
    json!({
        "Id": server_id,
        "ServerName": name,
        "Version": COMPAT_VERSION,
        "LocalAddress": base_url,
        "WanAddress": base_url
    })
}

pub fn user(user_id: &str, name: &str) -> Value {
    json!({
        "Id": user_id,
        "Name": name,
        "HasPassword": true,
        "HasConfiguredPassword": true,
        "Policy": {
            "IsAdministrator": true,
            "IsDisabled": false,
            "EnableRemoteAccess": true,
            "EnableMediaPlayback": true,
            "EnableContentDownloading": false
        },
        "Configuration": {
            "PlayDefaultAudioTrack": true,
            "EnableNextEpisodeAutoPlay": false
        }
    })
}

pub fn auth_response(token: &str, server_id: &str, authenticated_user: &Value) -> Value {
    json!({
        "AccessToken": token,
        "ServerId": server_id,
        "User": authenticated_user
    })
}

pub fn views(items: &[MediaItem], user_id: &str) -> Value {
    let entries: Vec<Value> = items
        .iter()
        .filter(|item| item.kind == ItemKind::Library)
        .map(|item| detail(item, user_id, 0))
        .collect();
    let total = entries.len();
    json!({ "Items": entries, "TotalRecordCount": total })
}

pub fn item_list(
    items: &[MediaItem],
    user_id: &str,
    progress: &HashMap<String, i64>,
    total: usize,
) -> Value {
    let entries: Vec<Value> = items
        .iter()
        .map(|item| {
            detail(
                item,
                user_id,
                progress.get(&item.id).copied().unwrap_or_default(),
            )
        })
        .collect();
    json!({ "Items": entries, "TotalRecordCount": total })
}

pub fn detail(item: &MediaItem, _user_id: &str, progress_ticks: i64) -> Value {
    let is_folder = !item.kind.is_video();
    let item_type = item.kind.emby_type();
    let mut result = json!({
        "Id": item.id,
        "Name": item.name,
        "SortName": item.metadata.sort_title.as_ref().unwrap_or(&item.name),
        "Type": item_type,
        "IsFolder": is_folder,
        "UserData": {
            "ItemId": item.id,
            "Key": item.id,
            "PlaybackPositionTicks": progress_ticks.max(0),
            "Played": false,
            "PlayCount": 0,
            "IsFavorite": false
        }
    });

    if let Some(parent_id) = &item.parent_id {
        result["ParentId"] = json!(parent_id);
    }
    if let Some(date) = &item.date_created {
        result["DateCreated"] = json!(date);
    }
    if item.kind == ItemKind::Library {
        result["CollectionType"] = json!(item.metadata.collection_type);
    }
    if item.kind.is_video() {
        result["MediaType"] = json!("Video");
        result["MediaSources"] = json!(media_sources(item, None));
        result["MediaSourceCount"] = json!(crate::versions::sources(item).len());
        let source = crate::versions::sources(item)[0];
        result["MediaStreams"] = json!(
            source
                .media_streams
                .iter()
                .map(|stream| media_stream(stream, &item.id, source, None))
                .collect::<Vec<_>>()
        );
        result["CanDownload"] = json!(false);
        if let Some(ticks) = item.runtime_ticks {
            result["RunTimeTicks"] = json!(ticks);
        }
    }
    let meta = &item.metadata;
    result["Genres"] = json!(meta.genres);
    result["Tags"] = json!(meta.tags);
    result["ProviderIds"] = json!(meta.provider_ids);
    for (key, value) in [
        ("OriginalTitle", json!(meta.original_title)),
        ("Overview", json!(meta.overview)),
        ("ProductionYear", json!(meta.year)),
        ("PremiereDate", json!(meta.premiere_date)),
        ("CommunityRating", json!(meta.community_rating)),
        ("OfficialRating", json!(meta.official_rating)),
        ("IndexNumber", json!(meta.index_number)),
        ("ParentIndexNumber", json!(meta.parent_index_number)),
        ("SeriesId", json!(meta.series_id)),
        ("SeriesName", json!(meta.series_name)),
        ("SeasonId", json!(meta.season_id)),
        ("SeasonName", json!(meta.season_name)),
    ] {
        if !value.is_null() {
            result[key] = value;
        }
    }
    if item.image_path.is_some() {
        result["ImageTags"] = json!({ "Primary": item.id });
        result["PrimaryImageTag"] = json!(item.id);
    }
    result
}

pub fn playback_info(item: &MediaItem, play_session_id: &str, stream_ticket: &str) -> Value {
    json!({
        "MediaSources": media_sources(item, Some((play_session_id, stream_ticket))),
        "PlaySessionId": play_session_id
    })
}

fn media_sources(item: &MediaItem, stream_access: Option<(&str, &str)>) -> Vec<Value> {
    crate::versions::sources(item)
        .into_iter()
        .map(|source| media_source(&item.id, source, stream_access))
        .collect()
}

fn media_source(item_id: &str, item: &MediaItem, stream_access: Option<(&str, &str)>) -> Value {
    let extension = item
        .container
        .as_deref()
        .map(|container| format!(".{container}"))
        .unwrap_or_default();
    let mut url = format!(
        "/Videos/{}/stream{}?Static=true&MediaSourceId={}",
        item_id, extension, item.id
    );
    if let Some((play_session_id, ticket)) = stream_access {
        url.push_str("&PlaySessionId=");
        url.push_str(
            &url::form_urlencoded::byte_serialize(play_session_id.as_bytes()).collect::<String>(),
        );
        url.push_str("&ticket=");
        url.push_str(&url::form_urlencoded::byte_serialize(ticket.as_bytes()).collect::<String>());
    }
    let mut result = json!({
        "Id": item.id,
        "Name": item.metadata.version_name.as_ref().unwrap_or(&item.name),
        "Protocol": "Http",
        "Type": "Default",
        "IsRemote": true,
        "Path": url,
        "DirectStreamUrl": url,
        "AddApiKeyToDirectStreamUrl": stream_access.is_none(),
        "SupportsDirectPlay": true,
        "SupportsDirectStream": false,
        "SupportsTranscoding": false,
        "MediaStreams": item.media_streams.iter().map(|stream| media_stream(stream, item_id, item, stream_access)).collect::<Vec<_>>(),
        "DefaultAudioStreamIndex": item.media_streams.iter().find(|stream| stream.stream_type == "Audio" && stream.is_default).or_else(|| item.media_streams.iter().find(|stream| stream.stream_type == "Audio")).map(|stream| stream.index),
        "DefaultSubtitleStreamIndex": item.media_streams.iter().find(|stream| stream.stream_type == "Subtitle" && (stream.is_default || stream.is_forced)).map(|stream| stream.index).unwrap_or(-1)
    });
    if let Some(size) = item.size {
        result["Size"] = json!(size);
    }
    if let Some(container) = &item.container {
        result["Container"] = json!(container);
    }
    if let Some(ticks) = item.runtime_ticks {
        result["RunTimeTicks"] = json!(ticks);
    }
    result
}

fn media_stream(
    stream: &crate::model::MediaStream,
    item_id: &str,
    source: &MediaItem,
    access: Option<(&str, &str)>,
) -> Value {
    let can_deliver = stream.is_external
        || (matches!(source.source, Some(crate::model::MediaSource::File { .. }))
            && crate::subtitles::text_codec(stream.codec.as_deref()));
    let mut result = json!({
        "Index": stream.index,
        "Type": stream.stream_type,
        "IsDefault": stream.is_default,
        "IsForced": stream.is_forced,
        "IsExternal": stream.is_external,
        "SupportsExternalStream": stream.stream_type == "Subtitle" && can_deliver
    });
    for (name, value) in [
        ("Codec", json!(stream.codec)),
        ("Width", json!(stream.width)),
        ("Height", json!(stream.height)),
        ("Channels", json!(stream.channels)),
        ("SampleRate", json!(stream.sample_rate)),
        ("Language", json!(stream.language)),
        ("Title", json!(stream.title)),
        (
            "DisplayTitle",
            json!(
                stream
                    .title
                    .as_ref()
                    .or(stream.language.as_ref())
                    .or(stream.codec.as_ref())
            ),
        ),
    ] {
        if !value.is_null() {
            result[name] = value;
        }
    }
    if stream.stream_type == "Subtitle" {
        let is_text = crate::subtitles::text_codec(stream.codec.as_deref());
        result["IsTextSubtitleStream"] = json!(is_text);
        if is_text && can_deliver {
            let format = match stream.codec.as_deref() {
                Some("ass" | "ssa") => "ass",
                Some("vtt" | "webvtt") => "vtt",
                _ => "srt",
            };
            let mut url = format!(
                "/Videos/{item_id}/{}/Subtitles/{}/Stream.{format}",
                source.id, stream.index
            );
            if let Some((_, ticket)) = access {
                url.push_str("?ticket=");
                url.push_str(
                    &url::form_urlencoded::byte_serialize(ticket.as_bytes()).collect::<String>(),
                );
            }
            result["DeliveryMethod"] = json!("External");
            result["DeliveryUrl"] = json!(url);
            result["AddApiKeyToDirectStreamUrl"] = json!(access.is_none());
        } else {
            result["DeliveryMethod"] = json!("Embed");
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MediaSource;

    fn video() -> MediaItem {
        MediaItem {
            id: "item-1".into(),
            parent_id: Some("library-1".into()),
            name: "Example".into(),
            kind: ItemKind::Video,
            library_name: "Movies".into(),
            relative_path: "Example.mp4".into(),
            source: Some(MediaSource::File {
                path: "/private/media/Example.mp4".into(),
            }),
            container: Some("mp4".into()),
            size: Some(100),
            image_path: None,
            date_created: None,
            runtime_ticks: Some(125_000_000),
            media_streams: Vec::new(),
            metadata: Default::default(),
            versions: Vec::new(),
        }
    }

    #[test]
    fn playback_uses_gateway_url_without_exposing_node_path() {
        let response = playback_info(&video(), "session-1", "ticket&value");
        let source = &response["MediaSources"][0];
        assert_eq!(source["SupportsTranscoding"], false);
        assert_eq!(source["SupportsDirectStream"], false);
        assert_eq!(source["Container"], "mp4");
        assert_eq!(
            source["DirectStreamUrl"],
            "/Videos/item-1/stream.mp4?Static=true&MediaSourceId=item-1&PlaySessionId=session-1&ticket=ticket%26value"
        );
        assert!(!response.to_string().contains("/private/media"));
    }

    #[test]
    fn item_detail_has_no_stream_ticket() {
        let details = detail(&video(), "user-1", 0);
        let source = &details["MediaSources"][0];
        assert_eq!(source["AddApiKeyToDirectStreamUrl"], true);
        assert!(
            !source["DirectStreamUrl"]
                .as_str()
                .unwrap()
                .contains("ticket=")
        );
    }

    #[test]
    fn item_list_preserves_total_and_resume_position() {
        let mut progress = HashMap::new();
        progress.insert("item-1".to_string(), 50_000_000);
        let response = item_list(&[video()], "user-1", &progress, 10);
        assert_eq!(response["TotalRecordCount"], 10);
        assert_eq!(
            response["Items"][0]["UserData"]["PlaybackPositionTicks"],
            50_000_000
        );
    }
}
