use std::{collections::BTreeMap, path::Path};

use uuid::Uuid;

use crate::model::MediaItem;

// Explicit markers avoid merging distinct films with similar names or provider IDs.
pub fn identify(item: &mut MediaItem, path: &Path) {
    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return;
    };
    let Some((base, label)) = stem
        .strip_suffix(']')
        .and_then(|stem| stem.rsplit_once(" [version="))
    else {
        return;
    };
    if base.trim().is_empty() || label.trim().is_empty() {
        return;
    }
    let parent = Path::new(&item.relative_path)
        .parent()
        .unwrap_or(Path::new(""));
    let key = format!(
        "emby-bridge:versions:{}/{}",
        item.library_name,
        parent.join(base).to_string_lossy().replace('\\', "/")
    );
    item.metadata.version_group =
        Some(Uuid::new_v5(&Uuid::NAMESPACE_URL, key.as_bytes()).to_string());
    item.metadata.version_name = Some(label.trim().to_owned());
    if item.name == stem {
        item.name = base.to_owned();
    }
}

pub fn group(items: Vec<MediaItem>) -> Vec<MediaItem> {
    let mut groups: BTreeMap<String, Vec<MediaItem>> = BTreeMap::new();
    let mut result = Vec::new();
    for item in items {
        if item.kind.is_video()
            && let Some(key) = &item.metadata.version_group
        {
            groups.entry(key.clone()).or_default().push(item);
        } else {
            result.push(item);
        }
    }
    for (id, mut sources) in groups {
        sources.sort_by(|a, b| {
            a.metadata
                .version_name
                .cmp(&b.metadata.version_name)
                .then(a.id.cmp(&b.id))
        });
        let mut item = sources[0].clone();
        item.id = id;
        item.metadata.version_name = None;
        item.versions = sources;
        result.push(item);
    }
    result
}

pub fn sources(item: &MediaItem) -> Vec<&MediaItem> {
    if item.versions.is_empty() {
        vec![item]
    } else {
        item.versions.iter().collect()
    }
}

pub fn source<'a>(item: &'a MediaItem, id: Option<&str>) -> Option<&'a MediaItem> {
    let sources = sources(item);
    match id {
        Some(id) => sources.into_iter().find(|source| source.id == id),
        None => sources.into_iter().next(),
    }
}
