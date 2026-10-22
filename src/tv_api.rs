use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, Uri},
    routing::get,
};
use serde_json::{Value, json};

use crate::{
    db::UserItemState,
    emby_dto,
    gateway::{GatewayState, check_user},
    model::{ItemKind, MediaItem},
    query::{self, Parameters, parameter},
};

type ApiResult = Result<Json<Value>, StatusCode>;

pub fn router() -> Router<GatewayState> {
    Router::new()
        .route("/Shows/{id}/Seasons", get(seasons))
        .route("/Shows/{id}/Episodes", get(episodes))
        .route("/Shows/NextUp", get(next_up))
}

fn authorize(
    state: &GatewayState,
    headers: &HeaderMap,
    uri: &Uri,
    query: &Parameters,
) -> Result<(), StatusCode> {
    check_user(
        state,
        headers,
        uri,
        parameter(query, "UserId").unwrap_or("1"),
    )
}

fn series(state: &GatewayState, id: &str) -> Result<MediaItem, StatusCode> {
    state
        .cluster
        .item(id)
        .filter(|item| item.kind == ItemKind::Series)
        .ok_or(StatusCode::NOT_FOUND)
}

async fn seasons(
    State(state): State<GatewayState>,
    Path(id): Path<String>,
    Query(mut query): Query<Parameters>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult {
    authorize(&state, &headers, &uri, &query)?;
    series(&state, &id)?;
    query::put(&mut query, "SeriesId", id);
    query::put(&mut query, "Recursive", "true".into());
    query::put(&mut query, "IncludeItemTypes", "Season".into());
    query::set_default(&mut query, "SortBy", "IndexNumber,SortName");
    let all = state.cluster.items();
    let mut result = state.item_list(&query, usize::MAX);
    for season in result["Items"].as_array_mut().expect("item list") {
        let id = season["Id"].as_str().unwrap_or_default();
        let episodes: Vec<_> = all
            .iter()
            .filter(|item| {
                item.kind == ItemKind::Episode && item.metadata.season_id.as_deref() == Some(id)
            })
            .collect();
        season["ChildCount"] = json!(episodes.len());
        season["RecursiveItemCount"] = json!(episodes.len());
    }
    Ok(Json(result))
}

async fn episodes(
    State(state): State<GatewayState>,
    Path(id): Path<String>,
    Query(mut query): Query<Parameters>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult {
    authorize(&state, &headers, &uri, &query)?;
    series(&state, &id)?;
    if let Some(season_id) = parameter(&query, "SeasonId") {
        state
            .cluster
            .item(season_id)
            .filter(|item| {
                item.kind == ItemKind::Season && item.metadata.series_id.as_deref() == Some(&id)
            })
            .ok_or(StatusCode::NOT_FOUND)?;
    }
    let season = parameter(&query, "Season")
        .map(str::parse::<u32>)
        .transpose()
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    query::put(&mut query, "SeriesId", id);
    query::put(&mut query, "Recursive", "true".into());
    query::put(&mut query, "IncludeItemTypes", "Episode".into());
    query::set_default(
        &mut query,
        "SortBy",
        "ParentIndexNumber,IndexNumber,SortName",
    );
    let states = state.db.user_states();
    let mut selected = query::select(state.cluster.items(), &states, &query);
    selected.retain(|item| {
        season.is_none_or(|number| item.metadata.parent_index_number == Some(number))
    });
    if let Some(start) = parameter(&query, "StartItemId") {
        let index = selected
            .iter()
            .position(|item| item.id == start)
            .ok_or(StatusCode::NOT_FOUND)?;
        selected.drain(..index);
    }
    Ok(Json(list(selected, &query, &states)))
}

fn list(
    items: Vec<MediaItem>,
    query: &Parameters,
    states: &HashMap<String, UserItemState>,
) -> Value {
    let (page, total) = query::page(items, query, usize::MAX);
    let mut result = emby_dto::item_list(&page, "1", &HashMap::new(), total);
    query::decorate(&mut result, states);
    result["StartIndex"] = json!(
        parameter(query, "StartIndex")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0)
    );
    result
}

fn select_next(
    items: Vec<MediaItem>,
    states: &HashMap<String, UserItemState>,
    include_first: bool,
) -> Vec<MediaItem> {
    let mut groups: HashMap<String, Vec<MediaItem>> = HashMap::new();
    let now = chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::now())
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    for item in items {
        if item.kind == ItemKind::Episode
            && item.metadata.parent_index_number != Some(0)
            && item
                .metadata
                .premiere_date
                .as_ref()
                .is_none_or(|date| date <= &now)
            && let Some(series) = &item.metadata.series_id
        {
            groups.entry(series.clone()).or_default().push(item);
        }
    }
    let mut result = Vec::new();
    for (_, mut episodes) in groups {
        episodes.sort_by(|a, b| {
            (
                a.metadata.parent_index_number,
                a.metadata.index_number,
                &a.id,
            )
                .cmp(&(
                    b.metadata.parent_index_number,
                    b.metadata.index_number,
                    &b.id,
                ))
        });
        let last_started = episodes.iter().rposition(|item| {
            states
                .get(&item.id)
                .is_some_and(|state| state.played || state.position_ticks > 0)
        });
        let candidate = if let Some(index) = last_started {
            episodes
                .into_iter()
                .skip(index)
                .find(|item| states.get(&item.id).is_none_or(|state| !state.played))
        } else if include_first {
            episodes
                .into_iter()
                .find(|item| states.get(&item.id).is_none_or(|state| !state.played))
        } else {
            None
        };
        if let Some(item) = candidate {
            result.push(item);
        }
    }
    result
}

async fn next_up(
    State(state): State<GatewayState>,
    Query(mut query): Query<Parameters>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult {
    authorize(&state, &headers, &uri, &query)?;
    if let Some(id) = parameter(&query, "SeriesId") {
        series(&state, id)?;
    }
    let include_first = query::flag(&query, "EnableFirstEpisode").unwrap_or(true);
    let all = state.cluster.items();
    let states = state.db.user_states();
    let mut activity: HashMap<&str, &str> = HashMap::new();
    for item in &all {
        if let (Some(series), Some(date)) = (
            item.metadata.series_id.as_deref(),
            states
                .get(&item.id)
                .and_then(|state| state.last_played.as_deref()),
        ) {
            activity
                .entry(series)
                .and_modify(|existing| *existing = (*existing).max(date))
                .or_insert(date);
        }
    }
    query::put(&mut query, "Recursive", "true".into());
    query::put(&mut query, "IncludeItemTypes", "Episode".into());
    let mut selected = select_next(
        query::select(all.clone(), &states, &query),
        &states,
        include_first,
    );
    selected.sort_by(|a, b| {
        let date = |item: &MediaItem| {
            item.metadata
                .series_id
                .as_deref()
                .and_then(|id| activity.get(id).copied())
        };
        date(b)
            .cmp(&date(a))
            .then_with(|| (&a.metadata.series_name, &a.id).cmp(&(&b.metadata.series_name, &b.id)))
    });
    Ok(Json(list(selected, &query, &states)))
}
