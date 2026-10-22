use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant, SystemTime},
};

use axum::http::HeaderMap;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const SESSION_IDLE: Duration = Duration::from_secs(30 * 60);

struct Session {
    id: String,
    identity: HashMap<String, String>,
    last_seen: Instant,
    last_activity: String,
    capabilities: Value,
    now_playing: Option<Value>,
    play_state: Value,
}

#[derive(Default)]
pub struct SessionRegistry {
    sessions: Mutex<HashMap<String, Session>>,
}

fn key(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn identity(headers: &HeaderMap) -> HashMap<String, String> {
    headers
        .get("X-Emby-Authorization")
        .or_else(|| headers.get("Authorization"))
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .trim_start_matches("MediaBrowser ")
        .split(',')
        .filter_map(|part| {
            let (name, value) = part.trim().split_once('=')?;
            let name = name.trim().to_ascii_lowercase();
            matches!(name.as_str(), "client" | "device" | "deviceid" | "version")
                .then(|| (name, value.trim().trim_matches('"').to_owned()))
        })
        .collect()
}

fn date_now() -> String {
    chrono::DateTime::<chrono::Utc>::from(SystemTime::now())
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

impl SessionRegistry {
    pub fn touch(&self, token: &str, headers: &HeaderMap) {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|_, session| session.last_seen.elapsed() < SESSION_IDLE);
        let token_key = key(token);
        let entry = sessions.entry(token_key).or_insert_with(|| Session {
            id: Uuid::new_v4().simple().to_string(),
            identity: identity(headers),
            last_seen: Instant::now(),
            last_activity: date_now(),
            capabilities: json!({"PlayableMediaTypes":["Video"],"SupportsMediaControl":false}),
            now_playing: None,
            play_state: json!({"PositionTicks":0,"IsPaused":false,"IsMuted":false,"CanSeek":true}),
        });
        entry.identity.extend(identity(headers));
        entry.last_seen = Instant::now();
        entry.last_activity = date_now();
    }

    pub fn remove(&self, token: &str) {
        self.sessions.lock().unwrap().remove(&key(token));
    }

    pub fn capabilities(&self, token: &str, capabilities: Value) {
        if let Some(session) = self.sessions.lock().unwrap().get_mut(&key(token)) {
            session.capabilities = capabilities;
        }
    }

    pub fn playback(&self, token: &str, item: Value, payload: &Value, stopped: bool) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(session) = sessions.get_mut(&key(token)) else {
            return false;
        };
        if stopped {
            if session.now_playing.as_ref().and_then(|item| item.get("Id")) == item.get("Id") {
                session.now_playing = None;
            }
            return false;
        }
        let changed =
            session.now_playing.as_ref().and_then(|item| item.get("Id")) != item.get("Id");
        session.now_playing = Some(item);
        for field in [
            "PositionTicks",
            "IsPaused",
            "IsMuted",
            "VolumeLevel",
            "PlaybackRate",
            "PlayMethod",
        ] {
            if let Some(value) = payload.get(field) {
                session.play_state[field] = value.clone();
            }
        }
        changed
    }

    pub fn list(&self, name: &str, server_id: &str) -> Vec<Value> {
        let sessions = self.sessions.lock().unwrap();
        let mut result: Vec<_> = sessions
            .iter()
            .filter(|(_, session)| session.last_seen.elapsed() < SESSION_IDLE)
            .map(|(key, session)| {
                let get = |field: &str, default: &str| {
                    session
                        .identity
                        .get(field)
                        .cloned()
                        .unwrap_or_else(|| default.to_owned())
                };
                let mut result = json!({
                    "Id":session.id,"ServerId":server_id,"UserId":"1","UserName":name,
                    "Client":get("client","Unknown"),"DeviceName":get("device","Unknown"),
                    "DeviceId":get("deviceid",&key[..16]),"ApplicationVersion":get("version",""),
                    "LastActivityDate":session.last_activity,"Capabilities":session.capabilities,
                    "PlayState":session.play_state,"SupportedCommands":[],
                    "SupportsRemoteControl":false,"AdditionalUsers":[]
                });
                if let Some(item) = &session.now_playing {
                    result["NowPlayingItem"] = item.clone();
                }
                result
            })
            .collect();
        result.sort_by(|a, b| a["Id"].as_str().cmp(&b["Id"].as_str()));
        result
    }

    pub fn current(&self, token: &str, name: &str, server_id: &str) -> Option<Value> {
        let id = self.sessions.lock().unwrap().get(&key(token))?.id.clone();
        self.list(name, server_id)
            .into_iter()
            .find(|session| session["Id"] == id)
    }
}
