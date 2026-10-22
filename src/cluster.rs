use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use serde::Serialize;

use crate::model::{CatalogSnapshot, MediaItem, NodeAdvert};

const FAILURE_BACKOFF: Duration = Duration::from_secs(15);

struct NodeRecord {
    base_url: String,
    weight: u32,
    revision: String,
    items: HashMap<String, MediaItem>,
    version_groups: HashMap<String, Vec<String>>,
    last_seen: Instant,
    failed_until: Option<Instant>,
    active: Arc<AtomicUsize>,
}

pub struct NodeRegistry {
    nodes: Mutex<HashMap<String, NodeRecord>>,
    ttl: Duration,
    policy: Arc<dyn RoutingPolicy>,
}

#[derive(Clone, Debug)]
pub struct SelectedNode {
    pub id: String,
    pub base_url: String,
    pub item: MediaItem,
}

#[derive(Clone, Debug)]
pub struct RouteCandidate {
    pub node_id: String,
    pub base_url: String,
    pub item: MediaItem,
    pub weight: u32,
    pub active_streams: usize,
}

pub trait RoutingPolicy: Send + Sync {
    /// Returns an index into candidates, or None to decline routing.
    fn choose(&self, candidates: &[RouteCandidate]) -> Option<usize>;
}

pub struct WeightedLeastActive;

impl RoutingPolicy for WeightedLeastActive {
    fn choose(&self, candidates: &[RouteCandidate]) -> Option<usize> {
        candidates
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                let score_a = (a.active_streams as u128 + 1) * b.weight.max(1) as u128;
                let score_b = (b.active_streams as u128 + 1) * a.weight.max(1) as u128;
                score_a
                    .cmp(&score_b)
                    .then_with(|| a.node_id.cmp(&b.node_id))
            })
            .map(|(index, _)| index)
    }
}

#[derive(Debug, Serialize)]
pub struct NodeStatus {
    pub id: String,
    pub base_url: String,
    pub weight: u32,
    pub revision: String,
    pub item_count: usize,
    pub active_streams: usize,
    pub healthy: bool,
    pub last_seen_secs: u64,
    pub backoff_remaining_secs: u64,
}

pub struct ActiveLease {
    active: Arc<AtomicUsize>,
}

impl Drop for ActiveLease {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::Relaxed);
    }
}

impl NodeRegistry {
    pub fn new(ttl: Duration) -> Self {
        Self::with_policy(ttl, Arc::new(WeightedLeastActive))
    }

    pub fn with_policy(ttl: Duration, policy: Arc<dyn RoutingPolicy>) -> Self {
        Self {
            nodes: Mutex::new(HashMap::new()),
            ttl,
            policy,
        }
    }

    /// Replaces one node's catalog. Returns true when its catalog revision changed.
    pub fn upsert(&self, advert: NodeAdvert) -> bool {
        let NodeAdvert {
            id,
            base_url,
            weight,
            catalog: CatalogSnapshot { revision, items },
        } = advert;
        let items: HashMap<String, MediaItem> = items
            .into_iter()
            .map(|item| (item.id.clone(), item))
            .collect();
        let mut version_groups: HashMap<String, Vec<String>> = HashMap::new();
        for item in items.values() {
            if item.kind.is_video()
                && let Some(group) = &item.metadata.version_group
            {
                version_groups
                    .entry(group.clone())
                    .or_default()
                    .push(item.id.clone());
            }
        }
        let mut nodes = self.nodes.lock().unwrap();
        let changed = nodes
            .get(&id)
            .is_none_or(|existing| existing.revision != revision);
        let active = nodes
            .get(&id)
            .map(|existing| Arc::clone(&existing.active))
            .unwrap_or_else(|| Arc::new(AtomicUsize::new(0)));
        let failed_until = nodes.get(&id).and_then(|existing| existing.failed_until);
        nodes.insert(
            id,
            NodeRecord {
                base_url,
                weight: weight.max(1),
                revision,
                items,
                version_groups,
                last_seen: Instant::now(),
                failed_until,
                active,
            },
        );
        changed
    }

    /// Returns false for an unknown node so the worker can resend its catalog.
    pub fn heartbeat(&self, id: &str) -> bool {
        let mut nodes = self.nodes.lock().unwrap();
        let Some(node) = nodes.get_mut(id) else {
            return false;
        };
        node.last_seen = Instant::now();
        true
    }

    /// Temporarily excludes a failed worker while its heartbeat may still be healthy.
    pub fn mark_failed(&self, id: &str) -> bool {
        let mut nodes = self.nodes.lock().unwrap();
        let Some(node) = nodes.get_mut(id) else {
            return false;
        };
        node.failed_until = Some(Instant::now() + FAILURE_BACKOFF);
        true
    }

    pub fn item(&self, id: &str) -> Option<MediaItem> {
        let nodes = self.nodes.lock().unwrap();
        if let Some(canonical) = canonical_item(&nodes, id) {
            return nodes
                .values()
                .any(|node| {
                    self.healthy(node)
                        && node
                            .items
                            .get(id)
                            .is_some_and(|item| compatible(item, canonical))
                })
                .then(|| canonical.clone());
        }
        let mut sources = BTreeMap::new();
        for node in nodes.values().filter(|node| self.healthy(node)) {
            for source_id in node.version_groups.get(id).into_iter().flatten() {
                let item = &node.items[source_id];
                if let Some(canonical) = canonical_item(&nodes, source_id)
                    && compatible(item, canonical)
                {
                    sources
                        .entry(source_id.clone())
                        .or_insert_with(|| canonical.clone());
                }
            }
        }
        crate::versions::group(sources.into_values().collect())
            .into_iter()
            .find(|item| item.id == id)
    }

    /// One logical item is returned even when several nodes advertise replicas.
    pub fn items(&self) -> Vec<MediaItem> {
        let nodes = self.nodes.lock().unwrap();
        let mut merged = BTreeMap::new();
        let mut ids: Vec<_> = nodes.keys().collect();
        ids.sort();
        for id in ids {
            let node = &nodes[id];
            for (item_id, item) in &node.items {
                merged
                    .entry(item_id.clone())
                    .or_insert_with(|| item.clone());
            }
        }
        merged.retain(|id, canonical| {
            nodes.values().any(|node| {
                self.healthy(node)
                    && node
                        .items
                        .get(id)
                        .is_some_and(|item| compatible(item, canonical))
            })
        });
        crate::versions::group(merged.into_values().collect())
    }

    /// Selects only a healthy replica that owns this item, counting this request as in flight.
    pub fn select(&self, item_id: &str) -> Option<(SelectedNode, ActiveLease)> {
        let nodes = self.nodes.lock().unwrap();
        let canonical = canonical_item(&nodes, item_id)?;
        let mut candidates: Vec<RouteCandidate> = nodes
            .iter()
            .filter_map(|(id, node)| {
                let item = node.items.get(item_id)?;
                (self.healthy(node) && compatible(item, canonical)).then(|| RouteCandidate {
                    node_id: id.clone(),
                    base_url: node.base_url.clone(),
                    item: item.clone(),
                    weight: node.weight,
                    active_streams: node.active.load(Ordering::Relaxed),
                })
            })
            .collect();
        candidates.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        let chosen = candidates.get(self.policy.choose(&candidates)?)?;
        let node = nodes.get(&chosen.node_id)?;
        node.active.fetch_add(1, Ordering::Relaxed);
        Some((
            SelectedNode {
                id: chosen.node_id.clone(),
                base_url: chosen.base_url.clone(),
                item: chosen.item.clone(),
            },
            ActiveLease {
                active: Arc::clone(&node.active),
            },
        ))
    }

    pub fn statuses(&self) -> Vec<NodeStatus> {
        let nodes = self.nodes.lock().unwrap();
        let mut statuses: Vec<_> = nodes
            .iter()
            .map(|(id, node)| NodeStatus {
                id: id.clone(),
                base_url: node.base_url.clone(),
                weight: node.weight,
                revision: node.revision.clone(),
                item_count: node.items.len(),
                active_streams: node.active.load(Ordering::Relaxed),
                healthy: self.healthy(node),
                last_seen_secs: node.last_seen.elapsed().as_secs(),
                backoff_remaining_secs: node.failed_until.map_or(0, |until| {
                    until.saturating_duration_since(Instant::now()).as_secs()
                }),
            })
            .collect();
        statuses.sort_by(|a, b| a.id.cmp(&b.id));
        statuses
    }

    fn healthy(&self, node: &NodeRecord) -> bool {
        node.last_seen.elapsed() <= self.ttl
            && node
                .failed_until
                .is_none_or(|until| Instant::now() >= until)
    }
}

fn canonical_item<'a>(nodes: &'a HashMap<String, NodeRecord>, id: &str) -> Option<&'a MediaItem> {
    nodes
        .iter()
        .filter(|(_, node)| node.items.contains_key(id))
        .min_by(|(id_a, _), (id_b, _)| id_a.cmp(id_b))
        .and_then(|(_, node)| node.items.get(id))
}

fn compatible(item: &MediaItem, canonical: &MediaItem) -> bool {
    !matches!((item.size, canonical.size), (Some(a), Some(b)) if a != b)
        && !matches!((&item.container, &canonical.container), (Some(a), Some(b)) if !a.eq_ignore_ascii_case(b))
        && item.metadata.version_group == canonical.metadata.version_group
        && item.media_streams.len() == canonical.media_streams.len()
        && item
            .media_streams
            .iter()
            .zip(&canonical.media_streams)
            .all(|(a, b)| {
                let mut a = a.clone();
                let mut b = b.clone();
                a.external_path = None;
                b.external_path = None;
                a == b
            })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ItemKind, MediaSource};

    fn item(id: &str) -> MediaItem {
        MediaItem {
            id: id.into(),
            parent_id: None,
            name: id.into(),
            kind: ItemKind::Video,
            library_name: "Movies".into(),
            relative_path: format!("{id}.mp4"),
            source: Some(MediaSource::File {
                path: format!("/media/{id}.mp4"),
            }),
            container: Some("mp4".into()),
            size: Some(42),
            image_path: None,
            date_created: None,
            runtime_ticks: None,
            media_streams: Vec::new(),
            metadata: Default::default(),
            versions: Vec::new(),
        }
    }

    fn advert(id: &str, weight: u32, items: Vec<MediaItem>) -> NodeAdvert {
        NodeAdvert {
            id: id.into(),
            base_url: format!("http://{id}:8097"),
            weight,
            catalog: CatalogSnapshot {
                revision: "one".into(),
                items,
            },
        }
    }

    #[test]
    fn merges_replicas_and_releases_active_request() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        registry.upsert(advert("a", 1, vec![item("shared"), item("only-a")]));
        registry.upsert(advert("b", 1, vec![item("shared")]));
        assert_eq!(registry.items().len(), 2);
        let (first, lease) = registry.select("shared").unwrap();
        assert_eq!(first.id, "a");
        assert_eq!(registry.select("shared").unwrap().0.id, "b");
        assert_eq!(registry.select("only-a").unwrap().0.id, "a");
        assert!(registry.select("missing").is_none());
        drop(lease);
        assert_eq!(registry.select("shared").unwrap().0.id, "a");
    }

    #[test]
    fn weighting_and_resync_preserve_active_count() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        registry.upsert(advert("a", 1, vec![item("x")]));
        registry.upsert(advert("b", 3, vec![item("x")]));
        let (selected, lease) = registry.select("x").unwrap();
        assert_eq!(selected.id, "b");
        assert!(!registry.upsert(advert("b", 3, vec![item("x")])));
        assert_eq!(registry.statuses()[1].active_streams, 1);
        drop(lease);
        assert_eq!(registry.statuses()[1].active_streams, 0);
        assert!(!registry.heartbeat("unknown"));
        assert!(registry.heartbeat("a"));
    }

    #[test]
    fn failed_node_backs_off_and_mismatched_replica_is_not_routed() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        registry.upsert(advert("a", 1, vec![item("x")]));
        let mut different = item("x");
        different.size = Some(100);
        registry.upsert(advert("b", 100, vec![different]));
        assert_eq!(registry.select("x").unwrap().0.id, "a");
        assert!(registry.mark_failed("a"));
        assert!(registry.select("x").is_none());
        assert!(!registry.statuses()[0].healthy);
        assert!(registry.item("x").is_none());
    }

    struct PickLast;

    impl RoutingPolicy for PickLast {
        fn choose(&self, candidates: &[RouteCandidate]) -> Option<usize> {
            candidates.len().checked_sub(1)
        }
    }

    #[test]
    fn routing_policy_can_be_replaced() {
        let registry = NodeRegistry::with_policy(Duration::from_secs(30), Arc::new(PickLast));
        registry.upsert(advert("a", 1, vec![item("x")]));
        registry.upsert(advert("b", 1, vec![item("x")]));
        assert_eq!(registry.select("x").unwrap().0.id, "b");
    }

    #[test]
    fn versions_across_nodes_are_not_interchangeable_replicas() {
        let registry = NodeRegistry::new(Duration::from_secs(30));
        let mut low = item("low");
        low.metadata.version_group = Some("film".into());
        low.metadata.version_name = Some("1080p".into());
        let mut high = item("high");
        high.metadata.version_group = Some("film".into());
        high.metadata.version_name = Some("4K".into());
        registry.upsert(advert("a", 1, vec![low]));
        registry.upsert(advert("b", 100, vec![high]));
        assert_eq!(registry.items().len(), 1);
        assert_eq!(registry.item("film").unwrap().versions.len(), 2);
        assert_eq!(registry.select("low").unwrap().0.id, "a");
        assert_eq!(registry.select("high").unwrap().0.id, "b");
    }
}
