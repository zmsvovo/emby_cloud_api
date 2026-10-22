use std::{env, net::SocketAddr, path::PathBuf, time::Duration};

use crate::model::LibraryRoot;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Gateway,
    Worker,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub role: Role,
    pub bind: SocketAddr,
    pub server_name: String,
    pub public_url: String,
    pub admin_name: String,
    pub admin_password: String,
    pub database: PathBuf,
    pub internal_key: String,
    pub node_id: String,
    pub gateway_url: String,
    pub advertise_url: String,
    pub weight: u32,
    pub libraries: Vec<LibraryRoot>,
    pub scan_interval: Duration,
    pub heartbeat_interval: Duration,
    pub node_ttl: Duration,
    pub hook_urls: Vec<String>,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let role = match value("EMBY_ROLE", "gateway").as_str() {
            "gateway" => Role::Gateway,
            "worker" => Role::Worker,
            other => return Err(format!("invalid EMBY_ROLE: {other}")),
        };
        let default_bind = match role {
            Role::Gateway => "0.0.0.0:8096",
            Role::Worker => "0.0.0.0:8097",
        };
        let bind = value("EMBY_BIND", default_bind)
            .parse()
            .map_err(|e| format!("invalid EMBY_BIND: {e}"))?;
        let internal_key = required("EMBY_INTERNAL_KEY")?;
        if internal_key.len() < 24 {
            return Err("EMBY_INTERNAL_KEY must be at least 24 characters".into());
        }
        let admin_password = if role == Role::Gateway {
            required("EMBY_ADMIN_PASSWORD")?
        } else {
            String::new()
        };
        let libraries = if role == Role::Worker {
            match env::var("EMBY_LIBRARIES_JSON") {
                Ok(json) => serde_json::from_str::<Vec<LibraryRoot>>(&json)
                    .map_err(|e| format!("invalid EMBY_LIBRARIES_JSON: {e}"))?,
                Err(_) => vec![LibraryRoot {
                    name: "Media".into(),
                    path: value("EMBY_MEDIA_ROOT", "/media"),
                }],
            }
        } else {
            Vec::new()
        };
        if role == Role::Worker && libraries.is_empty() {
            return Err("worker needs at least one library".into());
        }
        let node_id = value("EMBY_NODE_ID", "");
        let gateway_url = value("EMBY_GATEWAY_URL", "");
        let advertise_url = value("EMBY_ADVERTISE_URL", "");
        if role == Role::Worker
            && (node_id.is_empty() || gateway_url.is_empty() || advertise_url.is_empty())
        {
            return Err(
                "worker needs EMBY_NODE_ID, EMBY_GATEWAY_URL and EMBY_ADVERTISE_URL".into(),
            );
        }
        for (name, url) in [
            ("EMBY_GATEWAY_URL", &gateway_url),
            ("EMBY_ADVERTISE_URL", &advertise_url),
        ] {
            if !url.is_empty() && !url.starts_with("http://") && !url.starts_with("https://") {
                return Err(format!("{name} must be an HTTP(S) URL"));
            }
        }
        Ok(Self {
            role,
            bind,
            server_name: value("EMBY_SERVER_NAME", "Emby Bridge"),
            public_url: value("EMBY_PUBLIC_URL", ""),
            admin_name: value("EMBY_ADMIN_NAME", "admin"),
            admin_password,
            database: PathBuf::from(value("EMBY_DATABASE", "/data/bridge.db")),
            internal_key,
            node_id,
            gateway_url,
            advertise_url,
            weight: number("EMBY_NODE_WEIGHT", 1)?.clamp(1, u32::MAX as u64) as u32,
            libraries,
            scan_interval: Duration::from_secs(number("EMBY_SCAN_SECONDS", 60)?.max(5)),
            heartbeat_interval: Duration::from_secs(number("EMBY_HEARTBEAT_SECONDS", 10)?.max(2)),
            node_ttl: Duration::from_secs(number("EMBY_NODE_TTL_SECONDS", 35)?.max(5)),
            hook_urls: value("EMBY_HOOK_URLS", "")
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
        })
    }
}

fn value(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.into())
}

fn required(key: &str) -> Result<String, String> {
    env::var(key)
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{key} must be set"))
}

fn number(key: &str, default: u64) -> Result<u64, String> {
    value(key, &default.to_string())
        .parse()
        .map_err(|e| format!("invalid {key}: {e}"))
}
