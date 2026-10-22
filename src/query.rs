use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
};

use serde_json::{Value, json};

use crate::{
    db::UserItemState,
    model::{ItemKind, MediaItem},
};

pub const ROOT_ID: &str = "root";
pub type Parameters = HashMap<String, String>;

pub fn parameter<'a>(query: &'a Parameters, name: &str) -> Option<&'a str> {
    query
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

pub fn flag(query: &Parameters, name: &str) -> Option<bool> {
    parameter(query, name).and_then(|value| {
        if value.eq_ignore_ascii_case("true") {
            Some(true)
        } else if value.eq_ignore_ascii_case("false") {
            Some(false)
        } else {
            None
        }
    })
}

pub fn set_default(query: &mut Parameters, key: &str, value: &str) {
    if parameter(query, key).is_none() {
        query.insert(key.to_owned(), value.to_owned());
    }
}

pub fn put(query: &mut Parameters, key: &str, value: String) {
    query.retain(|existing, _| !existing.eq_ignore_ascii_case(key));
    query.insert(key.to_owned(), value);
}

fn in_csv(value: &str, csv: &str) -> bool {
    csv.split(',')
        .any(|part| part.trim().eq_ignore_ascii_case(value))
}

fn type_matches(item: &MediaItem, types: &str) -> bool {
    in_csv(item.kind.emby_type(), types)
        || (item.kind.is_video() && in_csv("Video", types))
        || (item.kind == ItemKind::Library && in_csv("Folder", types))
}

fn descendants(all: &[MediaItem], parent: &str) -> HashSet<String> {
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    for item in all {
        if let Some(parent) = item.parent_id.as_deref() {
            children.entry(parent).or_default().push(&item.id);
        }
    }
    let mut seen = HashSet::from([parent.to_owned()]);
    let mut pending = vec![parent];
    while let Some(id) = pending.pop() {
        for child in children.get(id).into_iter().flatten() {
            if seen.insert((*child).to_owned()) {
                pending.push(child);
            }
        }
    }
    seen.remove(parent);
    seen
}

fn metadata_matches(values: &[String], requested: &str) -> bool {
    requested.split(['|', ',']).any(|value| {
        values
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(value.trim()))
    })
}

pub fn select(
    all: Vec<MediaItem>,
    states: &HashMap<String, UserItemState>,
    query: &Parameters,
) -> Vec<MediaItem> {
    let parent = parameter(query, "ParentId").filter(|id| *id != ROOT_ID);
    let recursive = flag(query, "Recursive").unwrap_or(false);
    let descendants = parent.filter(|_| recursive).map(|id| descendants(&all, id));
    let ids = parameter(query, "Ids");
    let search = parameter(query, "SearchTerm").map(str::to_lowercase);
    let default_state = UserItemState::default();
    let mut items: Vec<_> = all
        .into_iter()
        .filter(|item| {
            let state = states.get(&item.id).unwrap_or(&default_state);
            let parent_matches = if let Some(parent) = parent {
                if recursive {
                    descendants
                        .as_ref()
                        .is_some_and(|ids| ids.contains(&item.id))
                } else {
                    item.parent_id.as_deref() == Some(parent)
                }
            } else {
                recursive || ids.is_some() || search.is_some() || item.kind == ItemKind::Library
            };
            parent_matches
                && ids.is_none_or(|ids| ids.split(',').any(|id| id.trim() == item.id))
                && search
                    .as_ref()
                    .is_none_or(|search| item.name.to_lowercase().contains(search))
                && parameter(query, "IncludeItemTypes")
                    .is_none_or(|types| type_matches(item, types))
                && parameter(query, "ExcludeItemTypes")
                    .is_none_or(|types| !type_matches(item, types))
                && parameter(query, "MediaTypes")
                    .is_none_or(|types| item.kind.is_video() && in_csv("Video", types))
                && flag(query, "IsFolder").is_none_or(|folder| folder != item.kind.is_video())
                && parameter(query, "SeriesId")
                    .is_none_or(|id| item.metadata.series_id.as_deref() == Some(id))
                && parameter(query, "SeasonId")
                    .is_none_or(|id| item.metadata.season_id.as_deref() == Some(id))
                && parameter(query, "Genres")
                    .is_none_or(|genres| metadata_matches(&item.metadata.genres, genres))
                && parameter(query, "Tags")
                    .is_none_or(|tags| metadata_matches(&item.metadata.tags, tags))
                && parameter(query, "Years").is_none_or(|years| {
                    item.metadata
                        .year
                        .is_some_and(|year| in_csv(&year.to_string(), years))
                })
                && flag(query, "IsFavorite").is_none_or(|favorite| favorite == state.favorite)
                && flag(query, "IsPlayed").is_none_or(|played| played == state.played)
                && parameter(query, "Filters").is_none_or(|filters| {
                    filters.split(',').all(|filter| {
                        match filter.trim().to_ascii_lowercase().as_str() {
                            "isfavorite" | "isfavoriteorlikes" => state.favorite,
                            "isplayed" => state.played,
                            "isunplayed" => !state.played,
                            "isfolder" => !item.kind.is_video(),
                            "isnotfolder" => item.kind.is_video(),
                            "isresumable" => {
                                item.kind.is_video()
                                    && state.position_ticks > 0
                                    && !state.played
                                    && !state.hidden_from_resume
                            }
                            _ => true,
                        }
                    })
                })
        })
        .collect();
    let fields: Vec<_> = parameter(query, "SortBy")
        .unwrap_or("SortName")
        .split(',')
        .collect();
    let orders: Vec<_> = parameter(query, "SortOrder")
        .unwrap_or("Ascending")
        .split(',')
        .collect();
    items.sort_by(|a, b| {
        let state_a = states.get(&a.id).unwrap_or(&default_state);
        let state_b = states.get(&b.id).unwrap_or(&default_state);
        for (index, field) in fields.iter().enumerate() {
            let comparison = match field.trim().to_ascii_lowercase().as_str() {
                "datecreated" | "dateadded" => a.date_created.cmp(&b.date_created),
                "dateplayed" => state_a.last_played.cmp(&state_b.last_played),
                "playcount" => state_a.play_count.cmp(&state_b.play_count),
                "runtime" => a.runtime_ticks.cmp(&b.runtime_ticks),
                "indexnumber" => a.metadata.index_number.cmp(&b.metadata.index_number),
                "parentindexnumber" => a
                    .metadata
                    .parent_index_number
                    .cmp(&b.metadata.parent_index_number),
                "premieredate" => a.metadata.premiere_date.cmp(&b.metadata.premiere_date),
                "productionyear" => a.metadata.year.cmp(&b.metadata.year),
                "isfavoriteorliked" => state_a.favorite.cmp(&state_b.favorite),
                "sortname" => a
                    .metadata
                    .sort_title
                    .as_ref()
                    .unwrap_or(&a.name)
                    .to_lowercase()
                    .cmp(
                        &b.metadata
                            .sort_title
                            .as_ref()
                            .unwrap_or(&b.name)
                            .to_lowercase(),
                    ),
                _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            };
            if comparison != Ordering::Equal {
                let descending = orders
                    .get(index)
                    .or_else(|| orders.first())
                    .is_some_and(|order| order.trim().eq_ignore_ascii_case("Descending"));
                return if descending {
                    comparison.reverse()
                } else {
                    comparison
                };
            }
        }
        a.id.cmp(&b.id)
    });
    items
}

pub fn page(
    items: Vec<MediaItem>,
    query: &Parameters,
    default_limit: usize,
) -> (Vec<MediaItem>, usize) {
    let total = items.len();
    let start = parameter(query, "StartIndex")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let limit = parameter(query, "Limit")
        .and_then(|value| value.parse().ok())
        .unwrap_or(default_limit);
    (items.into_iter().skip(start).take(limit).collect(), total)
}

pub fn user_data(id: &str, state: &UserItemState) -> Value {
    let mut data = json!({
        "ItemId": id, "Key": id, "PlaybackPositionTicks": state.position_ticks,
        "Played": state.played, "IsFavorite": state.favorite, "PlayCount": state.play_count,
        "HideFromResume": state.hidden_from_resume
    });
    if let Some(date) = &state.last_played {
        data["LastPlayedDate"] = json!(date);
    }
    data
}

pub fn decorate(value: &mut Value, states: &HashMap<String, UserItemState>) {
    if let Some(items) = value.get_mut("Items").and_then(Value::as_array_mut) {
        for item in items {
            decorate(item, states);
        }
    } else if let Some(items) = value.as_array_mut() {
        for item in items {
            decorate(item, states);
        }
    } else if let Some(id) = value.get("Id").and_then(Value::as_str).map(str::to_owned)
        && value.get("UserData").is_some()
    {
        value["UserData"] = user_data(&id, states.get(&id).unwrap_or(&UserItemState::default()));
    }
}
