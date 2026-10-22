use std::{sync::Arc, time::Duration};

use serde::Serialize;
use serde_json::Value;
use tracing::warn;

#[derive(Clone)]
pub struct HookBus {
    urls: Arc<Vec<String>>,
    client: reqwest::Client,
    role: &'static str,
    node_id: String,
}

#[derive(Serialize)]
struct HookEvent {
    event: String,
    role: &'static str,
    node_id: String,
    data: Value,
}

impl HookBus {
    pub fn new(urls: Vec<String>, role: &'static str, node_id: String) -> Self {
        Self {
            urls: Arc::new(urls),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("HTTP client"),
            role,
            node_id,
        }
    }

    // Hook delivery is observational and never delays scanning, routing or playback.
    pub fn emit(&self, event: &str, data: Value) {
        if self.urls.is_empty() {
            return;
        }
        let payload = HookEvent {
            event: event.into(),
            role: self.role,
            node_id: self.node_id.clone(),
            data,
        };
        for url in self.urls.iter().cloned() {
            let client = self.client.clone();
            let payload = serde_json::to_value(&payload).expect("hook payload");
            tokio::spawn(async move {
                if let Err(error) = client.post(&url).json(&payload).send().await {
                    warn!(%url, %error, "hook delivery failed");
                }
            });
        }
    }
}
