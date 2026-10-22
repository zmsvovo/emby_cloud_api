use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Path, Query, Request, State},
    http::{
        HeaderMap, HeaderValue, Method, Response, StatusCode, Uri,
        header::{
            ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, IF_RANGE,
            LAST_MODIFIED, LOCATION, RANGE,
        },
    },
    routing::{get, post},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tracing::warn;
use uuid::Uuid;

use crate::{
    cluster::{ActiveLease, NodeRegistry, SelectedNode},
    config::Config,
    db::Database,
    emby_dto,
    hooks::HookBus,
    model::NodeAdvert,
    query::{self, parameter},
    sessions::SessionRegistry,
    streaming::response,
};

#[derive(Clone)]
pub struct GatewayState {
    pub(crate) config: Arc<Config>,
    pub(crate) cluster: Arc<NodeRegistry>,
    pub(crate) db: Arc<Database>,
    pub(crate) sessions: Arc<SessionRegistry>,
    client: reqwest::Client,
    pub(crate) hooks: HookBus,
}

impl GatewayState {
    pub fn new(config: Arc<Config>) -> Result<Self, String> {
        let db = Arc::new(Database::open(&config.database)?);
        let cluster = Arc::new(NodeRegistry::new(config.node_ttl));
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            hooks: HookBus::new(config.hook_urls.clone(), "gateway", String::new()),
            config,
            cluster,
            db,
            sessions: Arc::new(SessionRegistry::default()),
            client,
        })
    }

    pub(crate) fn user(&self) -> Value {
        let mut user = emby_dto::user("1", &self.config.admin_name);
        if let Some(configuration) = self.db.json_setting("user:1:configuration") {
            user["Configuration"] = configuration;
        }
        user
    }

    pub(crate) fn decorate(&self, value: &mut Value) {
        query::decorate(value, &self.db.user_states());
    }

    pub(crate) fn item_list(&self, query: &HashMap<String, String>, limit: usize) -> Value {
        let states = self.db.user_states();
        let selected = query::select(self.cluster.items(), &states, query);
        let (page, total) = query::page(selected, query, limit);
        let mut result = emby_dto::item_list(&page, "1", &HashMap::new(), total);
        query::decorate(&mut result, &states);
        result["StartIndex"] = json!(
            parameter(query, "StartIndex")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0)
        );
        result
    }
}

pub fn router(state: GatewayState) -> Router {
    let internal = Router::new()
        .route("/internal/nodes", post(register_node))
        .route("/internal/nodes/{id}/heartbeat", post(heartbeat));
    let public = Router::new()
        .route("/System/Ping", get(|| async { "Emby Server" }))
        .route("/System/Info/Public", get(public_info))
        .route("/System/Info", get(system_info))
        .route("/Users/Public", get(public_users))
        .route("/Users/AuthenticateByName", post(login))
        .route("/Users/Me", get(me))
        .route("/Users/{user}", get(get_user))
        .route("/Users/{user}/Views", get(views))
        .route("/Users/{user}/Items", get(items))
        .route("/Users/{user}/Items/Latest", get(latest))
        .route("/Users/{user}/Items/Resume", get(resume))
        .route("/Users/{user}/Items/{id}", get(user_item))
        .route(
            "/Users/{user}/PlayedItems/{id}",
            post(mark_played).delete(mark_unplayed),
        )
        .route("/Items/{id}", get(item))
        .route(
            "/Items/{id}/PlaybackInfo",
            get(playback_info).post(playback_info),
        )
        .route("/Items/{id}/Images/{image_type}", get(image).head(image))
        .route(
            "/Items/{id}/Images/{image_type}/{index}",
            get(indexed_image).head(indexed_image),
        )
        .route("/Videos/{id}/stream", get(video).head(video))
        .route("/Videos/{id}/{tail}", get(video_tail).head(video_tail))
        .route(
            "/Videos/{id}/{source}/Subtitles/{index}/{tail}",
            get(subtitle).head(subtitle),
        )
        .route(
            "/Items/{id}/{source}/Subtitles/{index}/{tail}",
            get(subtitle).head(subtitle),
        )
        .route(
            "/Videos/{id}/{source}/Subtitles/{index}/{start}/{tail}",
            get(subtitle_offset).head(subtitle_offset),
        )
        .route(
            "/Items/{id}/{source}/Subtitles/{index}/{start}/{tail}",
            get(subtitle_offset).head(subtitle_offset),
        )
        .route("/Sessions/Playing", post(playing))
        .route("/Sessions/Playing/Progress", post(playing))
        .route("/Sessions/Playing/Stopped", post(playing))
        .route("/Sessions/Logout", post(logout))
        .route("/Bridge/Nodes", get(nodes))
        .merge(crate::tv_api::router())
        .merge(crate::client_api::router());
    Router::new()
        .route("/health", get(|| async { StatusCode::OK }))
        .merge(internal)
        .merge(public.clone())
        .nest("/emby", public)
        .fallback(unmatched)
        .with_state(state)
}

async fn unmatched(uri: Uri) -> StatusCode {
    tracing::info!(path = %uri.path(), "unhandled Emby API route");
    StatusCode::NOT_FOUND
}

pub(crate) fn authorized(state: &GatewayState, headers: &HeaderMap, uri: &Uri) -> bool {
    token(headers, uri).is_some_and(|token| {
        if state.db.has_token(&token) {
            state.sessions.touch(&token, headers);
            true
        } else {
            false
        }
    })
}

pub(crate) fn token(headers: &HeaderMap, uri: &Uri) -> Option<String> {
    if let Some(token) = headers
        .get("X-Emby-Token")
        .or_else(|| headers.get("X-MediaBrowser-Token"))
        .and_then(|value| value.to_str().ok())
    {
        return Some(token.to_owned());
    }
    let authorization = headers
        .get("Authorization")
        .or_else(|| headers.get("X-Emby-Authorization"))
        .and_then(|v| v.to_str().ok());
    if let Some(header) = authorization {
        for field in header.split(',') {
            let field = field.trim().split_whitespace().last().unwrap_or_default();
            if let Some(value) = field.strip_prefix("Token=") {
                return Some(value.trim_matches('"').to_owned());
            }
        }
    }
    uri.query().and_then(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(key, _)| key.eq_ignore_ascii_case("api_key"))
            .map(|(_, value)| value.into_owned())
    })
}

fn internal_authorized(state: &GatewayState, headers: &HeaderMap) -> bool {
    headers.get("X-Internal-Key").is_some_and(|value| {
        value
            .as_bytes()
            .ct_eq(state.config.internal_key.as_bytes())
            .into()
    })
}

async fn register_node(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    request: Request,
) -> StatusCode {
    if !internal_authorized(&state, &headers) {
        return StatusCode::UNAUTHORIZED;
    }
    let bytes = match to_bytes(request.into_body(), 32 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE,
    };
    let advert: NodeAdvert = match serde_json::from_slice(&bytes) {
        Ok(advert) => advert,
        Err(_) => return StatusCode::BAD_REQUEST,
    };
    if advert.id.is_empty()
        || advert.catalog.items.len() > 100_000
        || url::Url::parse(&advert.base_url)
            .ok()
            .is_none_or(|url| !matches!(url.scheme(), "http" | "https"))
    {
        return StatusCode::BAD_REQUEST;
    }
    let count = advert.catalog.items.len();
    let id = advert.id.clone();
    let changed = state.cluster.upsert(advert);
    state.hooks.emit(
        "node.registered",
        json!({"node_id":id,"items":count,"changed":changed}),
    );
    StatusCode::NO_CONTENT
}

async fn heartbeat(
    State(state): State<GatewayState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> StatusCode {
    if !internal_authorized(&state, &headers) {
        return StatusCode::UNAUTHORIZED;
    }
    if state.cluster.heartbeat(&id) {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn public_info(State(state): State<GatewayState>, headers: HeaderMap) -> Json<Value> {
    let base_url = if state.config.public_url.is_empty() {
        let host = headers
            .get("Host")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("localhost:8096");
        let scheme = headers
            .get("X-Forwarded-Proto")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("http");
        format!("{scheme}://{host}")
    } else {
        state.config.public_url.clone()
    };
    Json(emby_dto::public_info(
        state.db.server_id(),
        &state.config.server_name,
        &base_url,
    ))
}

async fn public_users(State(state): State<GatewayState>) -> Json<Value> {
    Json(
        json!([{"Id":"1","Name":state.config.admin_name,"HasPassword":true,
        "HasConfiguredPassword":true}]),
    )
}

async fn system_info(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<Value>, StatusCode> {
    if !authorized(&state, &headers, &uri) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let mut info = public_info(State(state), headers).await.0;
    info["OperatingSystem"] = json!(std::env::consts::OS);
    info["HasPendingRestart"] = json!(false);
    info["CanSelfRestart"] = json!(false);
    info["CanSelfUpdate"] = json!(false);
    Ok(Json(info))
}

async fn login(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    let username = body
        .get("Username")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let password = body
        .get("Pw")
        .or_else(|| body.get("Password"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let username_ok = username
        .as_bytes()
        .ct_eq(state.config.admin_name.as_bytes());
    let password_ok = password
        .as_bytes()
        .ct_eq(state.config.admin_password.as_bytes());
    if !bool::from(username_ok & password_ok) {
        state.hooks.emit("auth.failed", json!({"user":username}));
        return Err(StatusCode::UNAUTHORIZED);
    }
    let token = state
        .db
        .create_token()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    state.hooks.emit("auth.succeeded", json!({"user":username}));
    state.sessions.touch(&token, &headers);
    let mut result = emby_dto::auth_response(&token, state.db.server_id(), &state.user());
    result["SessionInfo"] = json!(state.sessions.current(
        &token,
        &state.config.admin_name,
        state.db.server_id()
    ));
    Ok(Json(result))
}

async fn me(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<Value>, StatusCode> {
    if !authorized(&state, &headers, &uri) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(Json(state.user()))
}

async fn get_user(
    State(state): State<GatewayState>,
    Path(user): Path<String>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<Value>, StatusCode> {
    check_user(&state, &headers, &uri, &user)?;
    Ok(Json(state.user()))
}

pub(crate) fn check_user(
    state: &GatewayState,
    headers: &HeaderMap,
    uri: &Uri,
    user: &str,
) -> Result<(), StatusCode> {
    if !authorized(state, headers, uri) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if user != "1" {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(())
}

async fn views(
    State(state): State<GatewayState>,
    Path(user): Path<String>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<Value>, StatusCode> {
    check_user(&state, &headers, &uri, &user)?;
    let mut result = emby_dto::views(&state.cluster.items(), &user);
    state.decorate(&mut result);
    Ok(Json(result))
}

async fn items(
    State(state): State<GatewayState>,
    Path(user): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<Value>, StatusCode> {
    check_user(&state, &headers, &uri, &user)?;
    Ok(Json(state.item_list(&query, usize::MAX)))
}

async fn latest(
    State(state): State<GatewayState>,
    Path(user): Path<String>,
    Query(mut query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<Value>, StatusCode> {
    check_user(&state, &headers, &uri, &user)?;
    query::set_default(&mut query, "Recursive", "true");
    query::set_default(&mut query, "IncludeItemTypes", "Video");
    query::set_default(&mut query, "SortBy", "DateCreated");
    query::set_default(&mut query, "SortOrder", "Descending");
    Ok(Json(state.item_list(&query, 20)["Items"].take()))
}

async fn resume(
    State(state): State<GatewayState>,
    Path(user): Path<String>,
    Query(mut query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<Value>, StatusCode> {
    check_user(&state, &headers, &uri, &user)?;
    query::set_default(&mut query, "Recursive", "true");
    query::set_default(&mut query, "SortBy", "DatePlayed");
    query::set_default(&mut query, "SortOrder", "Descending");
    let filters = format!(
        "{},IsResumable",
        parameter(&query, "Filters").unwrap_or_default()
    );
    query::put(&mut query, "Filters", filters);
    Ok(Json(state.item_list(&query, 20)))
}

async fn user_item(
    State(state): State<GatewayState>,
    Path((user, id)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<Value>, StatusCode> {
    check_user(&state, &headers, &uri, &user)?;
    if id.eq_ignore_ascii_case(query::ROOT_ID) {
        return Ok(Json(crate::client_api::root_item(&state)));
    }
    let item = state.cluster.item(&id).ok_or(StatusCode::NOT_FOUND)?;
    let mut result = emby_dto::detail(&item, &user, state.db.progress(&id));
    state.decorate(&mut result);
    Ok(Json(result))
}

async fn item(
    State(state): State<GatewayState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<Value>, StatusCode> {
    if !authorized(&state, &headers, &uri) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if id.eq_ignore_ascii_case(query::ROOT_ID) {
        return Ok(Json(crate::client_api::root_item(&state)));
    }
    let item = state.cluster.item(&id).ok_or(StatusCode::NOT_FOUND)?;
    let mut result = emby_dto::detail(&item, "1", state.db.progress(&id));
    state.decorate(&mut result);
    Ok(Json(result))
}

async fn playback_info(
    State(state): State<GatewayState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    uri: Uri,
    request: Request,
) -> Result<Json<Value>, StatusCode> {
    if !authorized(&state, &headers, &uri) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let item = state.cluster.item(&id).ok_or(StatusCode::NOT_FOUND)?;
    if !item.kind.is_video() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let ticket = state
        .db
        .create_stream_ticket(&id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut options: HashMap<String, String> = uri
        .query()
        .map(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect()
        })
        .unwrap_or_default();
    let body = to_bytes(request.into_body(), 128 * 1024)
        .await
        .map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?;
    if !body.is_empty() {
        let payload: Value = serde_json::from_slice(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
        let payload = payload.as_object().ok_or(StatusCode::BAD_REQUEST)?;
        for (key, value) in payload {
            if value.is_null() {
                continue;
            }
            if value.is_string() || value.is_number() || value.is_boolean() {
                query::put(
                    &mut options,
                    key,
                    value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| value.to_string()),
                );
            }
        }
    }
    let mut result = emby_dto::playback_info(&item, &Uuid::new_v4().to_string(), &ticket);
    let sources = result["MediaSources"]
        .as_array_mut()
        .expect("media sources");
    if let Some(id) = parameter(&options, "MediaSourceId").filter(|id| !id.is_empty()) {
        sources.retain(|source| source["Id"].as_str() == Some(id));
        if sources.is_empty() {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    for (key, stream_type, default_key) in [
        ("AudioStreamIndex", "Audio", "DefaultAudioStreamIndex"),
        (
            "SubtitleStreamIndex",
            "Subtitle",
            "DefaultSubtitleStreamIndex",
        ),
    ] {
        if let Some(index) = parameter(&options, key) {
            let index = index.parse::<i32>().map_err(|_| StatusCode::BAD_REQUEST)?;
            // Track numbers are source-specific; never silently select an unrelated track.
            sources.retain(|source| {
                (index == -1 && stream_type == "Subtitle")
                    || source["MediaStreams"].as_array().is_some_and(|streams| {
                        streams
                            .iter()
                            .any(|stream| stream["Type"] == stream_type && stream["Index"] == index)
                    })
            });
            if sources.is_empty() {
                return Err(StatusCode::BAD_REQUEST);
            }
            for source in sources.iter_mut() {
                source[default_key] = json!(index);
            }
        }
    }
    state.hooks.emit(
        "playback.prepared",
        json!({"item_id":id,"sources":sources.len()}),
    );
    Ok(Json(result))
}

async fn subtitle(
    State(state): State<GatewayState>,
    Path((id, source, index, tail)): Path<(String, String, i32, String)>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
) -> Response<Body> {
    relay_subtitle(state, id, source, index, 0, tail, method, headers, uri).await
}

async fn subtitle_offset(
    State(state): State<GatewayState>,
    Path((id, source, index, start, tail)): Path<(String, String, i32, i64, String)>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
) -> Response<Body> {
    relay_subtitle(state, id, source, index, start, tail, method, headers, uri).await
}

async fn relay_subtitle(
    state: GatewayState,
    id: String,
    source: String,
    index: i32,
    start: i64,
    tail: String,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
) -> Response<Body> {
    let Some(format) = tail.strip_prefix("Stream.") else {
        return response(StatusCode::NOT_FOUND, Body::empty());
    };
    if !matches!(format, "srt" | "vtt" | "ass") || start < 0 {
        return response(StatusCode::BAD_REQUEST, Body::empty());
    }
    relay_resource(
        state,
        id,
        RelayResource::Subtitle {
            source,
            index,
            start,
            format: format.into(),
        },
        method,
        headers,
        uri,
    )
    .await
}

enum RelayResource {
    Video,
    Image,
    Subtitle {
        source: String,
        index: i32,
        start: i64,
        format: String,
    },
}

async fn video(
    state: State<GatewayState>,
    path: Path<String>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
) -> Response<Body> {
    relay_item(state.0, path.0, false, method, headers, uri).await
}

async fn video_tail(
    State(state): State<GatewayState>,
    Path((id, tail)): Path<(String, String)>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
) -> Response<Body> {
    if !tail.starts_with("stream.") {
        return response(StatusCode::NOT_FOUND, Body::empty());
    }
    relay_item(state, id, false, method, headers, uri).await
}

async fn image(
    State(state): State<GatewayState>,
    Path((id, image_type)): Path<(String, String)>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
) -> Response<Body> {
    if !image_type.eq_ignore_ascii_case("Primary") {
        return response(StatusCode::NOT_FOUND, Body::empty());
    }
    relay_item(state, id, true, method, headers, uri).await
}

async fn indexed_image(
    State(state): State<GatewayState>,
    Path((id, image_type, index)): Path<(String, String, usize)>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
) -> Response<Body> {
    if index != 0 || !image_type.eq_ignore_ascii_case("Primary") {
        return response(StatusCode::NOT_FOUND, Body::empty());
    }
    relay_item(state, id, true, method, headers, uri).await
}

async fn relay_item(
    state: GatewayState,
    id: String,
    is_image: bool,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
) -> Response<Body> {
    relay_resource(
        state,
        id,
        if is_image {
            RelayResource::Image
        } else {
            RelayResource::Video
        },
        method,
        headers,
        uri,
    )
    .await
}

async fn relay_resource(
    state: GatewayState,
    id: String,
    resource: RelayResource,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
) -> Response<Body> {
    let is_image = matches!(resource, RelayResource::Image);
    let ticket_authorized = !is_image
        && uri.query().is_some_and(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .find(|(key, _)| key == "ticket")
                .is_some_and(|(_, ticket)| state.db.has_stream_ticket(&ticket, &id))
        });
    if !authorized(&state, &headers, &uri) && !ticket_authorized {
        return response(StatusCode::UNAUTHORIZED, Body::empty());
    }
    let Some(item) = state.cluster.item(&id) else {
        return response(StatusCode::NOT_FOUND, Body::empty());
    };
    let params: HashMap<String, String> = uri
        .query()
        .map(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect()
        })
        .unwrap_or_default();
    let requested_source = match &resource {
        RelayResource::Subtitle { source, .. } => Some(source.as_str()),
        RelayResource::Video => parameter(&params, "MediaSourceId").filter(|id| !id.is_empty()),
        RelayResource::Image => None,
    };
    let Some(source) = crate::versions::source(&item, requested_source) else {
        return response(StatusCode::NOT_FOUND, Body::empty());
    };
    let source_id = source.id.clone();
    let path = match &resource {
        RelayResource::Image => format!("image/{source_id}"),
        RelayResource::Video => format!("stream/{source_id}"),
        RelayResource::Subtitle {
            index,
            start,
            format,
            ..
        } => {
            if !source
                .media_streams
                .iter()
                .any(|stream| stream.index == *index && stream.stream_type == "Subtitle")
            {
                return response(StatusCode::NOT_FOUND, Body::empty());
            }
            format!("subtitle/{source_id}/{index}/{format}?start={start}")
        }
    };
    let attempts = state.cluster.statuses().len().clamp(1, 3);
    for attempt in 0..attempts {
        let Some((selected, lease)) = state.cluster.select(&source_id) else {
            return response(
                if attempt == 0 {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::BAD_GATEWAY
                },
                Body::empty(),
            );
        };
        if !is_image && !selected.item.kind.is_video() {
            return response(StatusCode::NOT_FOUND, Body::empty());
        }
        state.hooks.emit(
            "route.selected",
            json!({"item_id":id,"source_id":source_id,"node_id":selected.id,"image":is_image}),
        );
        let node_id = selected.id.clone();
        match relay_node(&state, selected, lease, &path, method.clone(), &headers).await {
            Ok(result) => return result,
            Err(()) => {
                state.cluster.mark_failed(&node_id);
                state
                    .hooks
                    .emit("route.failed", json!({"item_id":id,"node_id":node_id}));
            }
        }
    }
    response(StatusCode::BAD_GATEWAY, Body::empty())
}

async fn relay_node(
    state: &GatewayState,
    selected: SelectedNode,
    lease: ActiveLease,
    path: &str,
    method: Method,
    headers: &HeaderMap,
) -> Result<Response<Body>, ()> {
    let url = format!(
        "{}/internal/{path}",
        selected.base_url.trim_end_matches('/'),
    );
    let request_method =
        reqwest::Method::from_bytes(method.as_str().as_bytes()).expect("HTTP method");
    let mut request = state
        .client
        .request(request_method, url)
        .header("X-Internal-Key", &state.config.internal_key);
    for name in [RANGE, IF_RANGE] {
        if let Some(value) = headers.get(&name) {
            request = request.header(name.as_str(), value.as_bytes());
        }
    }
    let upstream = match tokio::time::timeout(Duration::from_secs(10), request.send()).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            warn!(node = %selected.id, %error, "worker stream failed");
            return Err(());
        }
        Err(_) => {
            warn!(node = %selected.id, "worker response timed out");
            return Err(());
        }
    };
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if status.is_server_error() || status == StatusCode::NOT_FOUND {
        warn!(node = %selected.id, %status, "worker rejected stream");
        return Err(());
    }
    let mut builder = Response::builder().status(status);
    for name in [
        CONTENT_TYPE,
        CONTENT_LENGTH,
        CONTENT_RANGE,
        ACCEPT_RANGES,
        LOCATION,
        ETAG,
        LAST_MODIFIED,
    ] {
        if let Some(value) = upstream.headers().get(&name) {
            if let Ok(value) = HeaderValue::from_bytes(value.as_bytes()) {
                builder = builder.header(name, value);
            }
        }
    }
    if method == Method::HEAD || status.is_redirection() {
        return Ok(builder.body(Body::empty()).expect("worker response"));
    }
    let stream = upstream.bytes_stream().map(move |chunk| {
        let _hold_lease = &lease;
        chunk
    });
    Ok(builder
        .body(Body::from_stream(stream))
        .expect("worker response"))
}

async fn playing(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    uri: Uri,
    Json(payload): Json<Value>,
) -> StatusCode {
    if !authorized(&state, &headers, &uri) {
        return StatusCode::UNAUTHORIZED;
    }
    let Some(id) = payload.get("ItemId").and_then(Value::as_str) else {
        return StatusCode::BAD_REQUEST;
    };
    let Some(item) = state.cluster.item(id) else {
        return StatusCode::NOT_FOUND;
    };
    if !item.kind.is_video() {
        return StatusCode::BAD_REQUEST;
    }
    let Some(source) = crate::versions::source(
        &item,
        payload
            .get("MediaSourceId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty()),
    ) else {
        return StatusCode::NOT_FOUND;
    };
    let ticks = payload
        .get("PositionTicks")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if payload.get("PositionTicks").is_some() && state.db.set_progress(id, ticks).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    let stopped = uri.path().ends_with("/Stopped");
    let current_token = token(&headers, &uri).expect("authorized token");
    let mut detail = emby_dto::detail(&item, "1", ticks);
    if let Some(runtime) = source.runtime_ticks {
        detail["RunTimeTicks"] = json!(runtime);
    }
    state.decorate(&mut detail);
    if state
        .sessions
        .playback(&current_token, detail, &payload, stopped)
        && state.db.record_play(id).is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    if stopped
        && source
            .runtime_ticks
            .is_some_and(|runtime| runtime > 0 && ticks as i128 * 10 >= runtime as i128 * 9)
        && state.db.set_played(id, true).is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    state.hooks.emit(
        "playback.progress",
        json!({"item_id":id,"source_id":source.id,"position_ticks":ticks}),
    );
    StatusCode::NO_CONTENT
}

async fn mark_played(
    State(state): State<GatewayState>,
    Path((user, id)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<Value>, StatusCode> {
    check_user(&state, &headers, &uri, &user)?;
    if state.cluster.item(&id).is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    state
        .db
        .set_played(&id, true)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    state
        .hooks
        .emit("user_data.changed", json!({"item_id":id,"played":true}));
    Ok(Json(query::user_data(
        &id,
        state
            .db
            .user_states()
            .get(&id)
            .unwrap_or(&Default::default()),
    )))
}

async fn mark_unplayed(
    State(state): State<GatewayState>,
    Path((user, id)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<Value>, StatusCode> {
    check_user(&state, &headers, &uri, &user)?;
    if state.cluster.item(&id).is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    state
        .db
        .set_played(&id, false)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    state
        .hooks
        .emit("user_data.changed", json!({"item_id":id,"played":false}));
    Ok(Json(query::user_data(
        &id,
        state
            .db
            .user_states()
            .get(&id)
            .unwrap_or(&Default::default()),
    )))
}

async fn logout(State(state): State<GatewayState>, headers: HeaderMap, uri: Uri) -> StatusCode {
    let Some(token) = token(&headers, &uri) else {
        return StatusCode::UNAUTHORIZED;
    };
    state.db.revoke_token(&token);
    state.sessions.remove(&token);
    StatusCode::NO_CONTENT
}

async fn nodes(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<Value>, StatusCode> {
    if !authorized(&state, &headers, &uri) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(Json(json!(state.cluster.statuses())))
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, path::PathBuf, time::Duration};

    use axum::{body::to_bytes, http::Request};
    use tower::ServiceExt;

    use super::*;
    use crate::{
        catalog::scan_roots,
        config::Role,
        model::{CatalogSnapshot, ItemKind, LibraryRoot, MediaItem, MediaSource},
        worker,
    };

    async fn api(
        app: &Router,
        method: &str,
        path: &str,
        token: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("X-Emby-Token", token)
            .header(CONTENT_TYPE, "application/json")
            .body(if body.is_null() {
                Body::empty()
            } else {
                Body::from(body.to_string())
            })
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn authenticates_and_browses_a_registered_node() {
        let temp = tempfile::tempdir().unwrap();
        let config = Arc::new(Config {
            role: Role::Gateway,
            bind: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            server_name: "Test".into(),
            public_url: String::new(),
            admin_name: "admin".into(),
            admin_password: "secret".into(),
            database: PathBuf::from(temp.path().join("bridge.db")),
            internal_key: "test-internal-key-long-enough".into(),
            node_id: String::new(),
            gateway_url: String::new(),
            advertise_url: String::new(),
            weight: 1,
            libraries: Vec::new(),
            scan_interval: Duration::from_secs(60),
            heartbeat_interval: Duration::from_secs(10),
            node_ttl: Duration::from_secs(35),
            hook_urls: Vec::new(),
        });
        let state = GatewayState::new(config).unwrap();
        let item = MediaItem {
            id: "video-1".into(),
            parent_id: Some("library-1".into()),
            name: "Example".into(),
            kind: ItemKind::Video,
            library_name: "Media".into(),
            relative_path: "Example.mp4".into(),
            source: Some(MediaSource::File {
                path: "/media/Example.mp4".into(),
            }),
            container: Some("mp4".into()),
            size: Some(42),
            image_path: None,
            date_created: None,
            runtime_ticks: None,
            media_streams: Vec::new(),
            metadata: Default::default(),
            versions: Vec::new(),
        };
        state.cluster.upsert(NodeAdvert {
            id: "node-1".into(),
            base_url: "http://127.0.0.1:1".into(),
            weight: 1,
            catalog: CatalogSnapshot {
                revision: "one".into(),
                items: vec![
                    MediaItem {
                        id: "library-1".into(),
                        parent_id: None,
                        name: "Media".into(),
                        kind: ItemKind::Library,
                        library_name: "Media".into(),
                        relative_path: String::new(),
                        source: None,
                        container: None,
                        size: None,
                        image_path: None,
                        date_created: None,
                        runtime_ticks: None,
                        media_streams: Vec::new(),
                        metadata: Default::default(),
                        versions: Vec::new(),
                    },
                    item,
                ],
            },
        });
        let app = router(state);
        let login = app
            .clone()
            .oneshot(
                Request::post("/Users/AuthenticateByName")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"Username":"admin","Pw":"secret"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::OK);
        let login: Value =
            serde_json::from_slice(&to_bytes(login.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        let token = login["AccessToken"].as_str().unwrap();
        assert!(login["SessionInfo"]["Id"].is_string());
        let (status, favorite) = api(
            &app,
            "POST",
            "/Users/1/FavoriteItems/video-1",
            token,
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(favorite["IsFavorite"], true);
        let (_, favorites) = api(
            &app,
            "GET",
            "/Items?Filters=IsFavorite&Limit=1",
            token,
            Value::Null,
        )
        .await;
        assert_eq!(favorites["TotalRecordCount"], 1);
        assert_eq!(favorites["Items"][0]["UserData"]["IsFavorite"], true);
        let (_, search) = api(
            &app,
            "GET",
            "/Search/Hints?SearchTerm=example",
            token,
            Value::Null,
        )
        .await;
        assert_eq!(search["SearchHints"][0]["Id"], "video-1");
        let (_, ancestors) = api(&app, "GET", "/Items/video-1/Ancestors", token, Value::Null).await;
        assert_eq!(ancestors[0]["Id"], "library-1");
        assert_eq!(ancestors[1]["Id"], query::ROOT_ID);
        let (status, _) = api(
            &app,
            "POST",
            "/DisplayPreferences/media?UserId=1&Client=test",
            token,
            json!({"ViewType":"List","CustomPrefs":{"mode":"compact"}}),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, prefs) = api(
            &app,
            "GET",
            "/DisplayPreferences/media?UserId=1&Client=test",
            token,
            Value::Null,
        )
        .await;
        assert_eq!(prefs["CustomPrefs"]["mode"], "compact");
        let (_, other_prefs) = api(
            &app,
            "GET",
            "/DisplayPreferences/media?UserId=1&Client=other",
            token,
            Value::Null,
        )
        .await;
        assert_eq!(other_prefs["ViewType"], "Poster");
        let (status, _) = api(
            &app,
            "POST",
            "/Sessions/Playing",
            token,
            json!({"ItemId":"video-1","PositionTicks":12}),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, sessions) = api(&app, "GET", "/Sessions", token, Value::Null).await;
        assert_eq!(sessions[0]["NowPlayingItem"]["Id"], "video-1");
        let (_, resume) = api(&app, "GET", "/Users/1/Items/Resume", token, Value::Null).await;
        assert_eq!(resume["TotalRecordCount"], 1);
        api(
            &app,
            "POST",
            "/Users/1/Items/video-1/HideFromResume?Hide=true",
            token,
            Value::Null,
        )
        .await;
        let (_, resume) = api(&app, "GET", "/Users/1/Items/Resume", token, Value::Null).await;
        assert_eq!(resume["TotalRecordCount"], 0);
        let (status, played) = api(
            &app,
            "POST",
            "/Users/1/PlayedItems/video-1",
            token,
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(played["Played"], true);
        assert_eq!(played["PlaybackPositionTicks"], 0);
        let views = app
            .clone()
            .oneshot(
                Request::get("/emby/Users/1/Views")
                    .header("X-Emby-Token", token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(views.status(), StatusCode::OK);
        let views: Value =
            serde_json::from_slice(&to_bytes(views.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(views["TotalRecordCount"], 1);
        let playback = app
            .oneshot(
                Request::post("/Items/video-1/PlaybackInfo")
                    .header("X-Emby-Token", token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(playback.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn relays_a_local_range_and_redirects_a_strm() {
        let temp = tempfile::tempdir().unwrap();
        let media = temp.path().join("media");
        std::fs::create_dir(&media).unwrap();
        std::fs::write(media.join("Local.mp4"), b"0123456789").unwrap();
        std::fs::write(
            media.join("Local.nfo"),
            "<movie><title>Local</title><genre>Drama</genre><year>2024</year></movie>",
        )
        .unwrap();
        std::fs::write(
            media.join("Remote.strm"),
            "https://example.test/video.mp4?signature=short-lived\n",
        )
        .unwrap();
        std::fs::write(media.join("Film [version=1080p].mp4"), b"low-version").unwrap();
        std::fs::write(media.join("Film [version=4K].mp4"), b"high-version").unwrap();
        std::fs::write(
            media.join("Film [version=4K].eng.forced.srt"),
            b"1\n00:00:00,000 --> 00:00:01,000\nHello\n",
        )
        .unwrap();
        std::fs::write(media.join("Remote.ffprobe.json"), r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"hevc"},{"index":1,"codec_type":"audio","codec_name":"aac","tags":{"language":"eng"}},{"index":2,"codec_type":"audio","codec_name":"aac","tags":{"language":"zho"},"disposition":{"default":1}},{"index":3,"codec_type":"subtitle","codec_name":"ass"}]}"#).unwrap();
        let show = media.join("Show");
        std::fs::create_dir(&show).unwrap();
        for number in 1..=2 {
            std::fs::write(
                show.join(format!("Show.S01E{number:02}.strm")),
                "https://example.test/video.mp4?signature=short-lived",
            )
            .unwrap();
        }
        let mut config = Config {
            role: Role::Gateway,
            bind: "127.0.0.1:0".parse().unwrap(),
            server_name: "Test".into(),
            public_url: String::new(),
            admin_name: "admin".into(),
            admin_password: "secret".into(),
            database: temp.path().join("bridge.db"),
            internal_key: "test-internal-key-long-enough".into(),
            node_id: String::new(),
            gateway_url: String::new(),
            advertise_url: String::new(),
            weight: 1,
            libraries: vec![LibraryRoot {
                name: "Media".into(),
                path: media.to_string_lossy().into_owned(),
            }],
            scan_interval: Duration::from_secs(60),
            heartbeat_interval: Duration::from_secs(10),
            node_ttl: Duration::from_secs(35),
            hook_urls: Vec::new(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let node_url = format!("http://{}", listener.local_addr().unwrap());
        config.role = Role::Worker;
        config.node_id = "node-1".into();
        config.advertise_url = node_url.clone();
        let worker = worker::router(worker::WorkerState::new(Arc::new(config.clone())).unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, worker).await.unwrap();
        });
        let mut catalog = scan_roots(&config.libraries).unwrap();
        for item in &mut catalog.items {
            item.runtime_ticks = match item.metadata.version_name.as_deref() {
                Some("1080p") => Some(100),
                Some("4K") => Some(1000),
                _ => item.runtime_ticks,
            };
        }
        let local_id = catalog
            .items
            .iter()
            .find(|item| item.name == "Local")
            .unwrap()
            .id
            .clone();
        let remote_id = catalog
            .items
            .iter()
            .find(|item| item.name == "Remote")
            .unwrap()
            .id
            .clone();
        let series_id = catalog
            .items
            .iter()
            .find(|item| item.kind == ItemKind::Series)
            .unwrap()
            .id
            .clone();
        let episode_id = catalog
            .items
            .iter()
            .find(|item| item.kind == ItemKind::Episode && item.metadata.index_number == Some(1))
            .unwrap()
            .id
            .clone();
        let second_id = catalog
            .items
            .iter()
            .find(|item| item.kind == ItemKind::Episode && item.metadata.index_number == Some(2))
            .unwrap()
            .id
            .clone();
        config.role = Role::Gateway;
        let state = GatewayState::new(Arc::new(config)).unwrap();
        state.cluster.upsert(NodeAdvert {
            id: "node-1".into(),
            base_url: node_url,
            weight: 1,
            catalog,
        });
        let token = state.db.create_token().unwrap();
        let app = router(state.clone());
        let film = state
            .cluster
            .items()
            .into_iter()
            .find(|item| item.name == "Film")
            .unwrap();
        assert_eq!(film.versions.len(), 2);
        let source = film
            .versions
            .iter()
            .find(|source| source.metadata.version_name.as_deref() == Some("4K"))
            .unwrap();
        let (status, playback) = api(
            &app,
            "POST",
            &format!("/Items/{}/PlaybackInfo", film.id),
            &token,
            json!({"MediaSourceId":source.id,"SubtitleStreamIndex":0}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(playback["MediaSources"].as_array().unwrap().len(), 1);
        assert_eq!(playback["MediaSources"][0]["Name"], "4K");
        let (status, _) = api(
            &app,
            "POST",
            "/Sessions/Playing/Stopped",
            &token,
            json!({"ItemId":film.id,"MediaSourceId":source.id,"PositionTicks":95}),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(!state.db.user_states()[&film.id].played);
        assert_eq!(
            playback["MediaSources"][0]["MediaStreams"][0]["IsForced"],
            true
        );
        assert!(!playback.to_string().contains(media.to_str().unwrap()));
        let playback_source = &playback["MediaSources"][0];
        for (url, expected) in [
            (
                playback_source["DirectStreamUrl"].as_str().unwrap(),
                "high-version",
            ),
            (
                playback_source["MediaStreams"][0]["DeliveryUrl"]
                    .as_str()
                    .unwrap(),
                "1\n00:00:00,000 --> 00:00:01,000\nHello\n",
            ),
        ] {
            let result = app
                .clone()
                .oneshot(Request::get(url).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(result.status(), StatusCode::OK);
            assert_eq!(to_bytes(result.into_body(), 1024).await.unwrap(), expected);
        }
        let subtitle_url = playback_source["MediaStreams"][0]["DeliveryUrl"]
            .as_str()
            .unwrap();
        let head = app
            .clone()
            .oneshot(Request::head(subtitle_url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(head.status(), StatusCode::OK);
        assert!(head.headers().contains_key(CONTENT_LENGTH));
        assert!(to_bytes(head.into_body(), 1024).await.unwrap().is_empty());
        let forged = playback_source["DirectStreamUrl"]
            .as_str()
            .unwrap()
            .replace(
                &format!("MediaSourceId={}", source.id),
                &format!("MediaSourceId={local_id}"),
            );
        assert_eq!(
            app.clone()
                .oneshot(Request::get(forged).body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        let (status, _) = api(
            &app,
            "GET",
            subtitle_url.split('?').next().unwrap(),
            "",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, audio) = api(
            &app,
            "POST",
            &format!("/Items/{remote_id}/PlaybackInfo"),
            &token,
            json!({"AudioStreamIndex":1,"SubtitleStreamIndex":-1}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(audio["MediaSources"][0]["DefaultAudioStreamIndex"], 1);
        assert_eq!(
            audio["MediaSources"][0]["MediaStreams"][2]["Language"],
            "zho"
        );
        assert_eq!(
            audio["MediaSources"][0]["MediaStreams"][3]["DeliveryMethod"],
            "Embed"
        );
        let (status, _) = api(
            &app,
            "POST",
            &format!("/Items/{remote_id}/PlaybackInfo"),
            &token,
            json!({"AudioStreamIndex":99}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = api(
            &app,
            "GET",
            &format!("/Shows/{series_id}/Seasons"),
            "",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (_, seasons) = api(
            &app,
            "GET",
            &format!("/emby/Shows/{series_id}/Seasons"),
            &token,
            Value::Null,
        )
        .await;
        assert_eq!(seasons["TotalRecordCount"], 1);
        assert_eq!(seasons["Items"][0]["Type"], "Season");
        assert_eq!(seasons["Items"][0]["ChildCount"], 2);
        let season_id = seasons["Items"][0]["Id"].as_str().unwrap();
        let (_, episodes) = api(
            &app,
            "GET",
            &format!("/Shows/{series_id}/Episodes?SeasonId={season_id}"),
            &token,
            Value::Null,
        )
        .await;
        assert_eq!(episodes["TotalRecordCount"], 2);
        assert_eq!(episodes["Items"][0]["Id"], episode_id);
        assert_eq!(episodes["Items"][0]["SeriesId"], series_id);
        let (_, next) = api(
            &app,
            "GET",
            "/Shows/NextUp?EnableFirstEpisode=false",
            &token,
            Value::Null,
        )
        .await;
        assert_eq!(next["TotalRecordCount"], 0);
        let (_, next) = api(&app, "GET", "/Shows/NextUp", &token, Value::Null).await;
        assert_eq!(next["Items"][0]["Id"], episode_id);
        state.db.set_played(&episode_id, true).unwrap();
        let (_, next) = api(&app, "GET", "/Shows/NextUp", &token, Value::Null).await;
        assert_eq!(next["Items"][0]["Id"], second_id);
        let (_, counts) = api(&app, "GET", "/Items/Counts", &token, Value::Null).await;
        assert_eq!(counts["MovieCount"], 1);
        assert_eq!(counts["SeriesCount"], 1);
        assert_eq!(counts["EpisodeCount"], 2);
        let (_, movies) = api(
            &app,
            "GET",
            "/Items?IncludeItemTypes=Movie&Genres=Drama&Years=2024",
            &token,
            Value::Null,
        )
        .await;
        assert_eq!(movies["TotalRecordCount"], 1);
        assert_eq!(movies["Items"][0]["Type"], "Movie");
        let (_, filters) = api(&app, "GET", "/Items/Filters", &token, Value::Null).await;
        assert_eq!(filters["Genres"], json!(["Drama"]));
        for (id, expected_status) in [
            (&local_id, StatusCode::PARTIAL_CONTENT),
            (&remote_id, StatusCode::FOUND),
            (&episode_id, StatusCode::FOUND),
        ] {
            let playback = app
                .clone()
                .oneshot(
                    Request::post(format!("/Items/{id}/PlaybackInfo"))
                        .header("X-Emby-Token", &token)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let playback: Value =
                serde_json::from_slice(&to_bytes(playback.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            let url = playback["MediaSources"][0]["DirectStreamUrl"]
                .as_str()
                .unwrap();
            let streamed = app
                .clone()
                .oneshot(
                    Request::get(url)
                        .header(RANGE, "bytes=2-5")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(streamed.status(), expected_status);
            if id == &local_id {
                assert_eq!(streamed.headers()[CONTENT_RANGE], "bytes 2-5/10");
                assert_eq!(
                    to_bytes(streamed.into_body(), usize::MAX).await.unwrap(),
                    "2345"
                );
            } else {
                assert_eq!(
                    streamed.headers()[LOCATION],
                    "https://example.test/video.mp4?signature=short-lived"
                );
            }
        }
        server.abort();
        let _ = server.await;
    }
}
