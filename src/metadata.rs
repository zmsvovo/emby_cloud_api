use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Read,
    path::Path,
};

use crate::model::{ItemKind, ItemMetadata, MediaItem};

const MAX_NFO_BYTES: u64 = 1024 * 1024;

pub fn apply_sidecar(item: &mut MediaItem, path: &Path, single_video: bool) {
    let candidates = if item.kind == ItemKind::Folder {
        vec![path.join("tvshow.nfo"), path.join("season.nfo")]
    } else if item.kind.is_video() {
        let mut candidates = vec![path.with_extension("nfo")];
        if single_video && let Some(parent) = path.parent() {
            // A shared movie.nfo must never rename several different videos.
            candidates.push(parent.join("movie.nfo"));
        }
        candidates
    } else {
        return;
    };
    for candidate in candidates {
        if !fs::symlink_metadata(&candidate)
            .is_ok_and(|meta| meta.is_file() && meta.len() <= MAX_NFO_BYTES)
        {
            continue;
        }
        let mut xml = String::new();
        let result = fs::File::open(&candidate)
            .and_then(|file| file.take(MAX_NFO_BYTES + 1).read_to_string(&mut xml));
        if result.is_err() || xml.len() as u64 > MAX_NFO_BYTES {
            tracing::warn!(path = %candidate.display(), "cannot read NFO");
            continue;
        }
        if let Err(error) = parse_nfo(item, &xml) {
            tracing::warn!(path = %candidate.display(), %error, "ignoring invalid NFO");
            continue;
        }
        break;
    }
}

fn parse_nfo(item: &mut MediaItem, xml: &str) -> Result<(), String> {
    // roxmltree rejects DTDs by default; no external entities or network access.
    let document = roxmltree::Document::parse_with_options(
        xml,
        roxmltree::ParsingOptions {
            nodes_limit: 50_000,
            ..Default::default()
        },
    )
    .map_err(|error| error.to_string())?;
    let root = document.root_element();
    let kind = match root.tag_name().name() {
        "movie" if item.kind.is_video() => ItemKind::Movie,
        "episodedetails" if item.kind.is_video() => ItemKind::Episode,
        "tvshow" if item.kind == ItemKind::Folder => ItemKind::Series,
        "season" if item.kind == ItemKind::Folder => ItemKind::Season,
        _ => return Err("unsupported NFO root for item".into()),
    };
    let text = |name: &str| -> Option<String> {
        root.children()
            .find(|node| node.has_tag_name(name))
            .and_then(|node| node.text())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let number = |name: &str| text(name).and_then(|value| value.parse::<u32>().ok());
    let list = |name: &str| {
        let mut seen = HashSet::new();
        root.children()
            .filter(|node| node.has_tag_name(name))
            .filter_map(|node| node.text())
            .map(str::trim)
            .filter(|value| !value.is_empty() && seen.insert(value.to_lowercase()))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    item.kind = kind;
    if let Some(title) = text("title") {
        item.name = title;
    }
    let meta = &mut item.metadata;
    meta.original_title = text("originaltitle");
    meta.sort_title = text("sorttitle");
    meta.overview = text("plot").or_else(|| text("outline"));
    meta.year = number("year").filter(|year| (1..=9999).contains(year));
    meta.premiere_date = text("aired")
        .or_else(|| text("premiered"))
        .and_then(|date| chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d").ok())
        .map(|date| format!("{date}T00:00:00Z"));
    meta.community_rating = text("rating")
        .and_then(|value| value.parse::<f32>().ok())
        .filter(|rating| rating.is_finite() && (0.0..=10.0).contains(rating));
    meta.official_rating = text("mpaa");
    meta.genres = list("genre");
    meta.tags = list("tag");
    meta.index_number = number(if item.kind == ItemKind::Season {
        "seasonnumber"
    } else {
        "episode"
    });
    meta.parent_index_number = if item.kind == ItemKind::Episode {
        number("season")
    } else {
        None
    };
    for node in root.children().filter(|node| node.has_tag_name("uniqueid")) {
        if let (Some(provider), Some(value)) = (node.attribute("type"), node.text()) {
            let key = match provider.to_ascii_lowercase().as_str() {
                "imdb" => "Imdb",
                "tmdb" => "Tmdb",
                "tvdb" => "Tvdb",
                _ => continue,
            };
            if !value.trim().is_empty() {
                meta.provider_ids.insert(key.into(), value.trim().into());
            }
        }
    }
    if item.runtime_ticks.is_none() {
        item.runtime_ticks = number("runtime")
            .map(i64::from)
            .and_then(|minutes| minutes.checked_mul(600_000_000))
            .filter(|ticks| *ticks > 0);
    }
    Ok(())
}

fn episode_numbers(name: &str) -> Option<(u32, u32)> {
    let bytes = name.as_bytes();
    for start in 0..bytes.len() {
        if !bytes[start].eq_ignore_ascii_case(&b's')
            || (start > 0 && bytes[start - 1].is_ascii_alphanumeric())
        {
            continue;
        }
        let mut end = start + 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end == start + 1 || end >= bytes.len() || !bytes[end].eq_ignore_ascii_case(&b'e') {
            continue;
        }
        let season = name[start + 1..end].parse().ok()?;
        let episode_start = end + 1;
        end = episode_start;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end > episode_start {
            return Some((season, name[episode_start..end].parse().ok()?));
        }
    }
    None
}

fn season_number(name: &str) -> Option<u32> {
    let name = name.trim().to_ascii_lowercase();
    if name == "specials" {
        return Some(0);
    }
    name.strip_prefix("season")
        .or_else(|| name.strip_prefix('s'))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse().ok())
}

fn ancestor_series<'a>(
    item: &MediaItem,
    by_id: &'a HashMap<String, MediaItem>,
) -> Option<&'a MediaItem> {
    let mut parent = item.parent_id.as_ref();
    let mut seen = HashSet::new();
    while let Some(id) = parent {
        if !seen.insert(id) {
            break;
        }
        let ancestor = by_id.get(id)?;
        if ancestor.kind == ItemKind::Series {
            return Some(ancestor);
        }
        parent = ancestor.parent_id.as_ref();
    }
    None
}

pub fn organize(items: &mut Vec<MediaItem>) {
    for item in items.iter_mut().filter(|item| item.kind.is_video()) {
        if item.kind == ItemKind::Movie {
            continue;
        }
        if let Some((season, episode)) =
            episode_numbers(item.relative_path.rsplit('/').next().unwrap_or_default())
        {
            item.kind = ItemKind::Episode;
            item.metadata.index_number.get_or_insert(episode);
            item.metadata.parent_index_number.get_or_insert(season);
        }
    }
    let episode_parents: HashSet<_> = items
        .iter()
        .filter(|item| item.kind == ItemKind::Episode)
        .filter_map(|item| item.parent_id.clone())
        .collect();
    for item in items
        .iter_mut()
        .filter(|item| item.kind == ItemKind::Folder)
    {
        if episode_parents.contains(&item.id)
            && let Some(number) = season_number(&item.name)
        {
            item.kind = ItemKind::Season;
            item.metadata.index_number = Some(number);
        }
    }
    let series_parents: HashSet<_> = items
        .iter()
        .filter(|item| matches!(item.kind, ItemKind::Episode | ItemKind::Season))
        .filter_map(|item| item.parent_id.clone())
        .collect();
    let known: HashMap<_, _> = items
        .iter()
        .map(|item| (item.id.clone(), item.clone()))
        .collect();
    for item in items.iter_mut() {
        if item.kind == ItemKind::Folder
            && series_parents.contains(&item.id)
            && ancestor_series(item, &known).is_none()
        {
            item.kind = ItemKind::Series;
        }
        if item.kind == ItemKind::Season && item.metadata.index_number.is_none() {
            item.metadata.index_number =
                season_number(item.relative_path.rsplit('/').next().unwrap_or_default());
        }
    }
    let by_id: HashMap<_, _> = items
        .iter()
        .map(|item| (item.id.clone(), item.clone()))
        .collect();
    let mut seasons = HashMap::new();
    for item in items
        .iter_mut()
        .filter(|item| item.kind == ItemKind::Season)
    {
        if let Some(series) = ancestor_series(item, &by_id) {
            item.metadata.series_id = Some(series.id.clone());
            item.metadata.series_name = Some(series.name.clone());
            if let Some(number) = item.metadata.index_number {
                seasons
                    .entry((series.id.clone(), number))
                    .or_insert_with(|| item.clone());
            }
        }
    }
    let mut virtual_seasons = Vec::new();
    for item in items
        .iter_mut()
        .filter(|item| item.kind == ItemKind::Episode)
    {
        let Some(series) = ancestor_series(item, &by_id) else {
            continue;
        };
        item.metadata.series_id = Some(series.id.clone());
        item.metadata.series_name = Some(series.name.clone());
        let number = item
            .metadata
            .parent_index_number
            .or_else(|| {
                item.parent_id
                    .as_ref()
                    .and_then(|id| by_id.get(id))
                    .filter(|parent| parent.kind == ItemKind::Season)
                    .and_then(|parent| parent.metadata.index_number)
            })
            .unwrap_or(1);
        let season = seasons
            .entry((series.id.clone(), number))
            .or_insert_with(|| {
                let id = uuid::Uuid::new_v5(
                    &uuid::Uuid::NAMESPACE_URL,
                    format!("emby-bridge:virtual-season:{}/{number}", series.id).as_bytes(),
                )
                .to_string();
                let season = MediaItem {
                    versions: Vec::new(),
                    id,
                    parent_id: Some(series.id.clone()),
                    name: if number == 0 {
                        "Specials".into()
                    } else {
                        format!("Season {number}")
                    },
                    kind: ItemKind::Season,
                    library_name: series.library_name.clone(),
                    relative_path: format!("{}/.bridge-season-{number}", series.relative_path),
                    source: None,
                    container: None,
                    size: None,
                    image_path: series.image_path.clone(),
                    date_created: series.date_created.clone(),
                    runtime_ticks: None,
                    media_streams: Vec::new(),
                    metadata: ItemMetadata {
                        index_number: Some(number),
                        series_id: Some(series.id.clone()),
                        series_name: Some(series.name.clone()),
                        ..Default::default()
                    },
                };
                virtual_seasons.push(season.clone());
                season
            });
        item.parent_id = Some(season.id.clone());
        item.metadata.parent_index_number = Some(number);
        item.metadata.season_id = Some(season.id.clone());
        item.metadata.season_name = Some(season.name.clone());
    }
    items.extend(virtual_seasons);
    let mut types: HashMap<String, (bool, bool, bool)> = HashMap::new();
    for item in items.iter().filter(|item| item.kind.is_video()) {
        let flags = types.entry(item.library_name.clone()).or_default();
        flags.0 |= item.kind == ItemKind::Episode;
        flags.1 |= item.kind == ItemKind::Movie;
        flags.2 |= item.kind == ItemKind::Video;
    }
    for item in items
        .iter_mut()
        .filter(|item| item.kind == ItemKind::Library)
    {
        item.metadata.collection_type = match types.get(&item.library_name) {
            Some((true, false, false)) => Some("tvshows".into()),
            Some((false, true, false)) => Some("movies".into()),
            _ => None,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_dtd_and_malformed_xml_and_keeps_filename_fallback() {
        let mut item = MediaItem {
            versions: Vec::new(),
            id: "one".into(),
            parent_id: None,
            name: "Fallback".into(),
            kind: ItemKind::Video,
            library_name: "Media".into(),
            relative_path: "Fallback.strm".into(),
            source: None,
            container: None,
            size: None,
            image_path: None,
            date_created: None,
            runtime_ticks: None,
            media_streams: Vec::new(),
            metadata: Default::default(),
        };
        assert!(parse_nfo(&mut item, "<movie>").is_err());
        assert!(
            parse_nfo(
                &mut item,
                "<!DOCTYPE movie [<!ENTITY x \"expanded\">]><movie><title>&x;</title></movie>"
            )
            .is_err()
        );
        assert_eq!(item.name, "Fallback");
        assert_eq!(item.kind, ItemKind::Video);
        assert_eq!(episode_numbers("Show.S00E02.1080p"), Some((0, 2)));
        assert_eq!(episode_numbers("Show.s02e10"), Some((2, 10)));
        assert_eq!(episode_numbers("Contains01e01"), None);
    }
}
