use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
};

use axum::{
    Router,
    body::Body,
    extract::{Path as RoutePath, Query, State},
    http::{
        HeaderMap, HeaderValue, Method, Response, StatusCode,
        header::{CONTENT_TYPE, LOCATION},
    },
    routing::get,
};
use serde_json::json;
use subtle::ConstantTimeEq;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::{
    catalog::scan_roots_with_cache,
    config::Config,
    hooks::HookBus,
    model::{CatalogSnapshot, MediaItem, NodeAdvert},
    probe::ProbeCache,
    resolver::{DefaultSourceResolver, ResolvedSource, SourceResolver},
    streaming::{local_file, response},
};

#[derive(Clone)]
pub struct WorkerState {
    config: Arc<Config>,
    catalog: Arc<RwLock<WorkerCatalog>>,
    client: reqwest::Client,
    hooks: HookBus,
    resolver: Arc<dyn SourceResolver>,
    probe_cache: Arc<Mutex<ProbeCache>>,
}

struct WorkerCatalog {
    snapshot: CatalogSnapshot,
    by_id: HashMap<String, MediaItem>,
}

impl WorkerCatalog {
    fn new(snapshot: CatalogSnapshot) -> Self {
        let by_id = snapshot
            .items
            .iter()
            .map(|item| (item.id.clone(), item.clone()))
            .collect();
        Self { snapshot, by_id }
    }
}

impl WorkerState {
    pub fn new(config: Arc<Config>) -> Result<Self, String> {
        Self::with_resolver(config, Arc::new(DefaultSourceResolver))
    }

    pub fn with_resolver(
        config: Arc<Config>,
        resolver: Arc<dyn SourceResolver>,
    ) -> Result<Self, String> {
        let catalog = scan_roots_with_cache(&config.libraries, &mut ProbeCache::without_probe())
            .map_err(|e| e.to_string())?;
        let hooks = HookBus::new(config.hook_urls.clone(), "worker", config.node_id.clone());
        Ok(Self {
            config,
            catalog: Arc::new(RwLock::new(WorkerCatalog::new(catalog))),
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|e| e.to_string())?,
            hooks,
            resolver,
            probe_cache: Arc::new(Mutex::new(ProbeCache::new())),
        })
    }
}

pub fn router(state: WorkerState) -> Router {
    Router::new()
        .route("/health", get(|| async { StatusCode::OK }))
        .route("/internal/stream/{id}", get(stream).head(stream))
        .route("/internal/image/{id}", get(image).head(image))
        .route(
            "/internal/subtitle/{id}/{index}/{format}",
            get(subtitle).head(subtitle),
        )
        .with_state(state)
}

pub async fn sync_loop(state: WorkerState) {
    tokio::spawn(scan_loop(state.clone()));
    let mut heartbeat = tokio::time::interval(state.config.heartbeat_interval);
    let mut registered_revision = String::new();
    loop {
        heartbeat.tick().await;
        let revision = state.catalog.read().await.snapshot.revision.clone();
        if registered_revision != revision {
            let snapshot = state.catalog.read().await.snapshot.clone();
            state.hooks.emit(
                "node.before_register",
                json!({"revision":snapshot.revision}),
            );
            let advert = NodeAdvert {
                id: state.config.node_id.clone(),
                base_url: state.config.advertise_url.clone(),
                weight: state.config.weight,
                catalog: snapshot.clone(),
            };
            match state
                .client
                .post(format!(
                    "{}/internal/nodes",
                    state.config.gateway_url.trim_end_matches('/')
                ))
                .header("X-Internal-Key", &state.config.internal_key)
                .json(&advert)
                .timeout(state.config.heartbeat_interval)
                .send()
                .await
            {
                Ok(result) if result.status().is_success() => {
                    registered_revision = snapshot.revision;
                    state.hooks.emit(
                        "node.registered",
                        json!({"revision":registered_revision,"items":snapshot.items.len()}),
                    );
                    info!(node = %state.config.node_id, "catalog registered");
                }
                Ok(result) => warn!(status = %result.status(), "node registration rejected"),
                Err(error) => warn!(%error, "node registration failed"),
            }
        } else {
            let result = state
                .client
                .post(format!(
                    "{}/internal/nodes/{}/heartbeat",
                    state.config.gateway_url.trim_end_matches('/'),
                    state.config.node_id
                ))
                .header("X-Internal-Key", &state.config.internal_key)
                .timeout(state.config.heartbeat_interval)
                .send()
                .await;
            match result {
                Ok(response) if response.status().is_success() => {}
                Ok(response) if response.status() == reqwest::StatusCode::NOT_FOUND => {
                    registered_revision.clear()
                }
                Ok(response) => warn!(status = %response.status(), "heartbeat rejected"),
                Err(error) => warn!(%error, "heartbeat failed"),
            }
        }
    }
}

async fn scan_loop(state: WorkerState) {
    let mut scanning = tokio::time::interval(state.config.scan_interval);
    loop {
        scanning.tick().await;
        let roots = state.config.libraries.clone();
        let probe_cache = Arc::clone(&state.probe_cache);
        state
            .hooks
            .emit("catalog.before_scan", json!({"libraries":roots.len()}));
        match tokio::task::spawn_blocking(move || {
            let mut cache = probe_cache.lock().unwrap();
            scan_roots_with_cache(&roots, &mut cache)
        })
        .await
        {
            Ok(Ok(snapshot)) => {
                let mut current = state.catalog.write().await;
                if current.snapshot.revision != snapshot.revision {
                    state.hooks.emit(
                        "catalog.changed",
                        json!({"revision":snapshot.revision,"items":snapshot.items.len()}),
                    );
                    *current = WorkerCatalog::new(snapshot);
                }
            }
            Ok(Err(error)) => warn!(%error, "catalog scan failed"),
            Err(error) => warn!(%error, "catalog scan task failed"),
        }
    }
}

async fn stream(
    State(state): State<WorkerState>,
    RoutePath(id): RoutePath<String>,
    method: Method,
    headers: HeaderMap,
) -> Response<Body> {
    if !internal_authorized(&state, &headers) {
        return response(StatusCode::UNAUTHORIZED, Body::empty());
    }
    let Some(item) = find_item(&state, &id).await else {
        return response(StatusCode::NOT_FOUND, Body::empty());
    };
    match state.resolver.resolve(&item) {
        Some(ResolvedSource::Local(source)) => {
            let Some(path) = verified_path(&state.config, &source) else {
                return response(StatusCode::FORBIDDEN, Body::empty());
            };
            state.hooks.emit("stream.local", json!({"item_id":id}));
            local_file(&path, &method, &headers).await
        }
        Some(ResolvedSource::Redirect(url)) => {
            let Ok(location) = HeaderValue::from_str(url.as_str()) else {
                return response(StatusCode::BAD_GATEWAY, Body::empty());
            };
            state.hooks.emit(
                "stream.redirect",
                json!({"item_id":id,"host":url.host_str()}),
            );
            let mut result = response(StatusCode::FOUND, Body::empty());
            result.headers_mut().insert(LOCATION, location);
            result
        }
        None => response(StatusCode::NOT_FOUND, Body::empty()),
    }
}

async fn image(
    State(state): State<WorkerState>,
    RoutePath(id): RoutePath<String>,
    method: Method,
    headers: HeaderMap,
) -> Response<Body> {
    if !internal_authorized(&state, &headers) {
        return response(StatusCode::UNAUTHORIZED, Body::empty());
    }
    let Some(item) = find_item(&state, &id).await else {
        return response(StatusCode::NOT_FOUND, Body::empty());
    };
    let Some(image_path) = item.image_path else {
        return response(StatusCode::NOT_FOUND, Body::empty());
    };
    let Some(path) = verified_path(&state.config, Path::new(&image_path)) else {
        return response(StatusCode::FORBIDDEN, Body::empty());
    };
    let mut result = local_file(&path, &method, &headers).await;
    if result.status().is_success() && result.headers().get(CONTENT_TYPE).is_none() {
        result
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
    }
    result
}

async fn find_item(state: &WorkerState, id: &str) -> Option<MediaItem> {
    state.catalog.read().await.by_id.get(id).cloned()
}

async fn subtitle(
    State(state): State<WorkerState>,
    RoutePath((id, index, format)): RoutePath<(String, i32, String)>,
    Query(query): Query<HashMap<String, String>>,
    method: Method,
    headers: HeaderMap,
) -> Response<Body> {
    if !internal_authorized(&state, &headers) {
        return response(StatusCode::UNAUTHORIZED, Body::empty());
    }
    let Some(item) = find_item(&state, &id).await else {
        return response(StatusCode::NOT_FOUND, Body::empty());
    };
    let Some(stream) = item
        .media_streams
        .iter()
        .find(|stream| stream.index == index && stream.stream_type == "Subtitle")
    else {
        return response(StatusCode::NOT_FOUND, Body::empty());
    };
    let path = if stream.is_external {
        stream.external_path.as_deref()
    } else {
        match &item.source {
            Some(crate::model::MediaSource::File { path }) => Some(path.as_str()),
            _ => None,
        }
    };
    let Some(path) = path.and_then(|path| verified_path(&state.config, Path::new(path))) else {
        return response(StatusCode::NOT_FOUND, Body::empty());
    };
    let start = match query
        .get("start")
        .map(|value| value.parse::<i64>())
        .transpose()
    {
        Ok(value) => value.unwrap_or(0),
        Err(_) => return response(StatusCode::BAD_REQUEST, Body::empty()),
    };
    state.hooks.emit(
        "subtitle.requested",
        json!({"item_id":id,"index":index,"format":format}),
    );
    crate::subtitles::serve(&item, stream, &path, &format, start, &method, &headers).await
}

fn internal_authorized(state: &WorkerState, headers: &HeaderMap) -> bool {
    headers.get("X-Internal-Key").is_some_and(|value| {
        value
            .as_bytes()
            .ct_eq(state.config.internal_key.as_bytes())
            .into()
    })
}

fn verified_path(config: &Config, path: &Path) -> Option<std::path::PathBuf> {
    let resolved = path.canonicalize().ok()?;
    config
        .libraries
        .iter()
        .any(|root| {
            Path::new(&root.path)
                .canonicalize()
                .is_ok_and(|library| resolved.starts_with(library))
        })
        .then_some(resolved)
}
