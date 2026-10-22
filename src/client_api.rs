use std::collections::{HashMap, HashSet};

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, Method, StatusCode, Uri},
    routing::{get, post},
};
use serde_json::{Value, json};

use crate::{
    db::UserDataPatch,
    emby_dto,
    gateway::{GatewayState, authorized, check_user, token},
    model::ItemKind,
    query::{self, Parameters, parameter},
};

type ApiResult = Result<Json<Value>, StatusCode>;

pub fn router() -> Router<GatewayState> {
    Router::new()
        .route("/Users", get(users))
        .route("/Items", get(items))
        .route("/Users/{user}/Items/Root", get(root))
        .route("/Items/Counts", get(counts))
        .route("/Items/Filters", get(filters))
        .route("/Items/{id}/Ancestors", get(ancestors))
        .route("/Items/{id}/Similar", get(similar))
        .route("/Items/{id}/Images", get(images))
        .route("/Search/Hints", get(search))
        .route("/Library/VirtualFolders", get(virtual_folders))
        .route(
            "/Users/{user}/FavoriteItems/{id}",
            post(favorite).delete(favorite),
        )
        .route(
            "/Users/{user}/Items/{id}/UserData",
            get(user_data).post(update_user_data),
        )
        .route("/Users/{user}/Items/{id}/HideFromResume", post(hide_resume))
        .route("/Users/{user}/Configuration", post(configuration))
        .route(
            "/DisplayPreferences/{id}",
            get(preferences).post(save_preferences),
        )
        .route("/Sessions", get(sessions))
        .route("/Sessions/Capabilities", post(legacy_capabilities))
        .route("/Sessions/Capabilities/Full", post(capabilities))
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

pub fn root_item(state: &GatewayState) -> Value {
    json!({"Id":query::ROOT_ID,"Name":state.config.server_name,"Type":"Folder","IsFolder":true,
        "ChildCount":state.cluster.items().iter().filter(|item| item.kind == ItemKind::Library).count(),
        "UserData":query::user_data(query::ROOT_ID, &Default::default())})
}

async fn root(
    State(state): State<GatewayState>,
    Path(user): Path<String>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult {
    check_user(&state, &headers, &uri, &user)?;
    Ok(Json(root_item(&state)))
}

async fn users(State(state): State<GatewayState>, headers: HeaderMap, uri: Uri) -> ApiResult {
    if !authorized(&state, &headers, &uri) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(Json(json!([state.user()])))
}

async fn items(
    State(state): State<GatewayState>,
    Query(mut query): Query<Parameters>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult {
    authorize(&state, &headers, &uri, &query)?;
    query::set_default(&mut query, "Recursive", "true");
    Ok(Json(state.item_list(&query, usize::MAX)))
}

async fn counts(
    State(state): State<GatewayState>,
    Query(query): Query<Parameters>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult {
    authorize(&state, &headers, &uri, &query)?;
    let mut query = query;
    query::set_default(&mut query, "Recursive", "true");
    let items = query::select(state.cluster.items(), &state.db.user_states(), &query);
    Ok(Json(json!({
        "ItemCount":items.len(),"VideoCount":items.iter().filter(|item| item.kind.is_video()).count(),
        "MovieCount":items.iter().filter(|item| item.kind == ItemKind::Movie).count(),
        "SeriesCount":items.iter().filter(|item| item.kind == ItemKind::Series).count(),
        "EpisodeCount":items.iter().filter(|item| item.kind == ItemKind::Episode).count(),
        "TrailerCount":0,"MusicVideoCount":0,
        "SongCount":0,"AlbumCount":0,"ArtistCount":0,"BoxSetCount":0,"BookCount":0,"GameCount":0
    })))
}

async fn filters(
    State(state): State<GatewayState>,
    Query(query): Query<Parameters>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult {
    authorize(&state, &headers, &uri, &query)?;
    let mut query = query;
    query::set_default(&mut query, "Recursive", "true");
    let items = query::select(state.cluster.items(), &state.db.user_states(), &query);
    let mut genres = std::collections::BTreeSet::new();
    let mut tags = std::collections::BTreeSet::new();
    let mut years = std::collections::BTreeSet::new();
    for item in items {
        genres.extend(item.metadata.genres);
        tags.extend(item.metadata.tags);
        years.extend(item.metadata.year);
    }
    Ok(Json(json!({"Genres":genres,"Tags":tags,"Years":years})))
}

async fn ancestors(
    State(state): State<GatewayState>,
    Path(id): Path<String>,
    Query(query): Query<Parameters>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult {
    authorize(&state, &headers, &uri, &query)?;
    if id == query::ROOT_ID {
        return Ok(Json(json!([])));
    }
    let all: HashMap<_, _> = state
        .cluster
        .items()
        .into_iter()
        .map(|item| (item.id.clone(), item))
        .collect();
    let item = all.get(&id).ok_or(StatusCode::NOT_FOUND)?;
    let mut parent = item.parent_id.as_deref();
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    while let Some(id) = parent {
        if !seen.insert(id) {
            break;
        }
        let Some(item) = all.get(id) else { break };
        result.push(emby_dto::detail(item, "1", 0));
        parent = item.parent_id.as_deref();
    }
    result.push(root_item(&state));
    let mut result = json!(result);
    state.decorate(&mut result);
    Ok(Json(result))
}

async fn similar(
    State(state): State<GatewayState>,
    Path(id): Path<String>,
    Query(mut query): Query<Parameters>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult {
    authorize(&state, &headers, &uri, &query)?;
    let item = state.cluster.item(&id).ok_or(StatusCode::NOT_FOUND)?;
    let states = state.db.user_states();
    query::put(&mut query, "Recursive", "false".into());
    query::put(&mut query, "IncludeItemTypes", "Video".into());
    if let Some(parent) = item.parent_id {
        query::put(&mut query, "ParentId", parent);
    }
    let selected = query::select(state.cluster.items(), &states, &query)
        .into_iter()
        .filter(|item| item.id != id)
        .collect();
    let (page, total) = query::page(selected, &query, 20);
    let mut result = emby_dto::item_list(&page, "1", &HashMap::new(), total);
    query::decorate(&mut result, &states);
    Ok(Json(result))
}

async fn images(
    State(state): State<GatewayState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult {
    if !authorized(&state, &headers, &uri) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let item = state.cluster.item(&id).ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(if item.image_path.is_some() {
        json!([{"ImageType":"Primary","ImageIndex":0,"ImageTag":id}])
    } else {
        json!([])
    }))
}

async fn search(
    State(state): State<GatewayState>,
    Query(mut query): Query<Parameters>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult {
    authorize(&state, &headers, &uri, &query)?;
    if parameter(&query, "SearchTerm").is_none_or(|value| value.trim().is_empty()) {
        return Err(StatusCode::BAD_REQUEST);
    }
    query::put(&mut query, "Recursive", "true".into());
    let selected = query::select(state.cluster.items(), &state.db.user_states(), &query);
    let (page, total) = query::page(selected, &query, 20);
    let hints: Vec<_> = page.iter().map(|item| {
        let detail = emby_dto::detail(item, "1", 0);
        let mut hint = json!({"Id":item.id,"ItemId":item.id,"Name":item.name,"Type":detail["Type"],
            "MediaType":detail["MediaType"],"RunTimeTicks":item.runtime_ticks});
        if item.image_path.is_some() {
            hint["PrimaryImageTag"] = json!(item.id);
            hint["PrimaryImageItemId"] = json!(item.id);
        }
        hint
    }).collect();
    Ok(Json(json!({"SearchHints":hints,"TotalRecordCount":total})))
}

async fn virtual_folders(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult {
    if !authorized(&state, &headers, &uri) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(Json(
        json!(state.cluster.items().iter().filter(|item| item.kind == ItemKind::Library)
        .map(|item| json!({"Name":item.name,"ItemId":item.id,"Locations":[],"CollectionType":item.metadata.collection_type,
            "LibraryOptions":{"EnableRealtimeMonitor":false}})).collect::<Vec<_>>()),
    ))
}

fn item_user_data(state: &GatewayState, id: &str) -> Value {
    query::user_data(
        id,
        state
            .db
            .user_states()
            .get(id)
            .unwrap_or(&Default::default()),
    )
}

fn check_item(
    state: &GatewayState,
    headers: &HeaderMap,
    uri: &Uri,
    user: &str,
    id: &str,
) -> Result<(), StatusCode> {
    check_user(state, headers, uri, user)?;
    state.cluster.item(id).ok_or(StatusCode::NOT_FOUND)?;
    Ok(())
}

async fn favorite(
    State(state): State<GatewayState>,
    Path((user, id)): Path<(String, String)>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult {
    check_item(&state, &headers, &uri, &user, &id)?;
    state
        .db
        .set_favorite(&id, method == Method::POST)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    state.hooks.emit(
        "user_data.changed",
        json!({"item_id":id,"favorite":method == Method::POST}),
    );
    Ok(Json(item_user_data(&state, &id)))
}

async fn user_data(
    State(state): State<GatewayState>,
    Path((user, id)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult {
    check_item(&state, &headers, &uri, &user, &id)?;
    Ok(Json(item_user_data(&state, &id)))
}

async fn update_user_data(
    State(state): State<GatewayState>,
    Path((user, id)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
    Json(patch): Json<UserDataPatch>,
) -> ApiResult {
    check_item(&state, &headers, &uri, &user, &id)?;
    if patch
        .playback_position_ticks
        .is_some_and(|position| position < 0)
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    state
        .db
        .update_user_data(&id, &patch)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    state.hooks.emit("user_data.changed", json!({"item_id":id}));
    Ok(Json(item_user_data(&state, &id)))
}

async fn hide_resume(
    State(state): State<GatewayState>,
    Path((user, id)): Path<(String, String)>,
    Query(query): Query<Parameters>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<StatusCode, StatusCode> {
    check_item(&state, &headers, &uri, &user, &id)?;
    state
        .db
        .set_hidden_from_resume(&id, query::flag(&query, "Hide").unwrap_or(true))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    state.hooks.emit(
        "user_data.changed",
        json!({"item_id":id,"hidden_from_resume":query::flag(&query,"Hide").unwrap_or(true)}),
    );
    Ok(StatusCode::NO_CONTENT)
}

async fn configuration(
    State(state): State<GatewayState>,
    Path(user): Path<String>,
    headers: HeaderMap,
    uri: Uri,
    Json(body): Json<Value>,
) -> Result<StatusCode, StatusCode> {
    check_user(&state, &headers, &uri, &user)?;
    let object = body.as_object().ok_or(StatusCode::BAD_REQUEST)?;
    let mut configuration = state.user()["Configuration"].clone();
    configuration
        .as_object_mut()
        .expect("configuration object")
        .extend(object.clone());
    state
        .db
        .set_json_setting("user:1:configuration", &configuration)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    state
        .hooks
        .emit("user.configuration.updated", json!({"user_id":"1"}));
    Ok(StatusCode::NO_CONTENT)
}

fn preferences_key(id: &str, query: &Parameters) -> String {
    format!(
        "preferences:{}",
        json!([id, "1", parameter(query, "Client").unwrap_or_default()])
    )
}

async fn preferences(
    State(state): State<GatewayState>,
    Path(id): Path<String>,
    Query(query): Query<Parameters>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult {
    authorize(&state, &headers, &uri, &query)?;
    Ok(Json(state.db.json_setting(&preferences_key(&id, &query)).unwrap_or_else(|| json!({
        "Id":id,"ViewType":"Poster","SortBy":"SortName","SortOrder":"Ascending",
        "IndexBy":null,"RememberIndexing":false,"RememberSorting":false,
        "ScrollDirection":"Horizontal","ShowBackdrop":true,"ShowSidebar":false,
        "ImageType":"Primary","PrimaryImageHeight":250,"PrimaryImageWidth":250,"CustomPrefs":{}
    }))))
}

async fn save_preferences(
    State(state): State<GatewayState>,
    Path(id): Path<String>,
    Query(query): Query<Parameters>,
    headers: HeaderMap,
    uri: Uri,
    Json(mut body): Json<Value>,
) -> Result<StatusCode, StatusCode> {
    authorize(&state, &headers, &uri, &query)?;
    if !body.is_object() {
        return Err(StatusCode::BAD_REQUEST);
    }
    body["Id"] = json!(id);
    state
        .db
        .set_json_setting(&preferences_key(&id, &query), &body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    state.hooks.emit(
        "preferences.updated",
        json!({"id":id,"client":parameter(&query,"Client")}),
    );
    Ok(StatusCode::NO_CONTENT)
}

async fn sessions(
    State(state): State<GatewayState>,
    Query(query): Query<Parameters>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult {
    authorize(&state, &headers, &uri, &query)?;
    let sessions: Vec<_> = state
        .sessions
        .list(&state.config.admin_name, state.db.server_id())
        .into_iter()
        .filter(|session| {
            parameter(&query, "DeviceId").is_none_or(|device| session["DeviceId"] == device)
        })
        .collect();
    Ok(Json(json!(sessions)))
}

async fn capabilities(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    uri: Uri,
    Json(body): Json<Value>,
) -> Result<StatusCode, StatusCode> {
    if !authorized(&state, &headers, &uri) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if !body.is_object() {
        return Err(StatusCode::BAD_REQUEST);
    }
    state
        .sessions
        .capabilities(&token(&headers, &uri).expect("authorized token"), body);
    Ok(StatusCode::NO_CONTENT)
}

async fn legacy_capabilities(
    State(state): State<GatewayState>,
    Query(query): Query<Parameters>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<StatusCode, StatusCode> {
    if !authorized(&state, &headers, &uri) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let media: Vec<_> = parameter(&query, "PlayableMediaTypes")
        .unwrap_or("Video")
        .split(',')
        .collect();
    state.sessions.capabilities(&token(&headers, &uri).expect("authorized token"), json!({
        "PlayableMediaTypes":media,"SupportsMediaControl":query::flag(&query,"SupportsMediaControl").unwrap_or(false)
    }));
    Ok(StatusCode::NO_CONTENT)
}
