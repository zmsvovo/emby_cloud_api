use std::{
    collections::HashSet,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

use crate::model::{CatalogSnapshot, ItemKind, LibraryRoot, MediaItem, MediaSource};
use crate::probe::{ProbeCache, ProbeData};

const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mkv", "m4v", "mov", "avi", "webm", "ts"];

pub fn is_video_extension(extension: &str) -> bool {
    VIDEO_EXTENSIONS
        .iter()
        .any(|known| extension.eq_ignore_ascii_case(known))
}

#[cfg(test)]
pub fn scan_roots(roots: &[LibraryRoot]) -> io::Result<CatalogSnapshot> {
    scan_roots_with_cache(roots, &mut ProbeCache::new())
}

pub fn scan_roots_with_cache(
    roots: &[LibraryRoot],
    cache: &mut ProbeCache,
) -> io::Result<CatalogSnapshot> {
    cache.begin_scan();
    let mut items = Vec::new();
    let mut names = HashSet::new();

    for root in roots {
        if !names.insert(root.name.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("duplicate library name: {}", root.name),
            ));
        }

        let library_id = item_id(&root.name, "");
        items.push(MediaItem {
            id: library_id.clone(),
            parent_id: None,
            name: root.name.clone(),
            kind: ItemKind::Library,
            library_name: root.name.clone(),
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
        });

        let configured_path = Path::new(&root.path);
        let root_path = match configured_path.canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                tracing::warn!(library = %root.name, path = %root.path, "library root is missing");
                continue;
            }
            Err(error) => return Err(error),
        };
        if !root_path.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("library root is not a directory: {}", root.path),
            ));
        }
        items.last_mut().expect("library item exists").image_path =
            folder_image(&root_path).map(path_string);

        let mut pending = vec![(root_path, String::new(), library_id)];
        while let Some((directory, parent_relative, parent_id)) = pending.pop() {
            let mut entries = fs::read_dir(&directory)?.collect::<io::Result<Vec<_>>>()?;
            entries.sort_by_key(|entry| entry.file_name());
            let single_video = entries
                .iter()
                .filter(|entry| {
                    entry
                        .path()
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| {
                            is_video_extension(ext) || ext.eq_ignore_ascii_case("strm")
                        })
                })
                .take(2)
                .count()
                == 1;

            for entry in &entries {
                let file_type = entry.file_type()?;
                if file_type.is_symlink() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let relative = if parent_relative.is_empty() {
                    name.clone()
                } else {
                    format!("{parent_relative}/{name}")
                };
                let path = entry.path();

                if file_type.is_dir() {
                    let id = item_id(&root.name, &relative);
                    items.push(MediaItem {
                        id: id.clone(),
                        parent_id: Some(parent_id.clone()),
                        name,
                        kind: ItemKind::Folder,
                        library_name: root.name.clone(),
                        relative_path: relative.clone(),
                        source: None,
                        container: None,
                        size: None,
                        image_path: folder_image(&path).map(path_string),
                        date_created: file_date(&entry.metadata()?),
                        runtime_ticks: None,
                        media_streams: Vec::new(),
                        metadata: Default::default(),
                        versions: Vec::new(),
                    });
                    crate::metadata::apply_sidecar(items.last_mut().unwrap(), &path, false);
                    pending.push((path, relative, id));
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }

                let extension = path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(str::to_ascii_lowercase);
                let (source, container) = match extension.as_deref() {
                    Some("strm") => {
                        let Some(target) = read_strm(&path)? else {
                            tracing::warn!(path = %path.display(), "skipping invalid STRM file");
                            continue;
                        };
                        let container = Url::parse(&target).ok().and_then(|url| {
                            Path::new(url.path())
                                .extension()
                                .and_then(|ext| ext.to_str())
                                .map(str::to_ascii_lowercase)
                        });
                        (MediaSource::Strm { target }, container)
                    }
                    Some(ext) if VIDEO_EXTENSIONS.contains(&ext) => (
                        MediaSource::File {
                            path: path_string(path.clone()),
                        },
                        Some(ext.to_owned()),
                    ),
                    _ => continue,
                };

                let metadata = entry.metadata()?;
                let probe = if matches!(source, MediaSource::File { .. }) {
                    cache.get(&path, &metadata)
                } else {
                    read_probe_sidecar(&path).unwrap_or_default()
                };
                let display_name = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or(&name)
                    .to_owned();
                items.push(MediaItem {
                    id: item_id(&root.name, &relative),
                    parent_id: Some(parent_id.clone()),
                    name: display_name,
                    kind: ItemKind::Video,
                    library_name: root.name.clone(),
                    relative_path: relative,
                    source: Some(source),
                    container,
                    size: if extension.as_deref() == Some("strm") {
                        None
                    } else {
                        Some(metadata.len())
                    },
                    image_path: video_image(&path).map(path_string),
                    date_created: file_date(&metadata),
                    runtime_ticks: probe.runtime_ticks,
                    media_streams: probe.media_streams,
                    metadata: Default::default(),
                    versions: Vec::new(),
                });
                crate::metadata::apply_sidecar(items.last_mut().unwrap(), &path, single_video);
                let item = items.last_mut().unwrap();
                crate::versions::identify(item, &path);
                crate::subtitles::discover(item, &path, &entries)?;
            }
        }
    }

    crate::metadata::organize(&mut items);
    items.sort_by(|a, b| {
        (&a.library_name, &a.relative_path).cmp(&(&b.library_name, &b.relative_path))
    });
    let mut digest = Sha256::new();
    for item in &items {
        update_digest(&mut digest, &item.id);
        update_digest(&mut digest, &item.relative_path);
        update_digest(&mut digest, &item.name);
        update_digest(
            &mut digest,
            item.date_created.as_deref().unwrap_or_default(),
        );
        update_digest(&mut digest, &format!("{:?}", item.kind));
        if let Some(source) = &item.source {
            match source {
                MediaSource::File { path } => {
                    update_digest(&mut digest, "file");
                    update_digest(&mut digest, path);
                    update_file_metadata(&mut digest, Path::new(path))?;
                }
                MediaSource::Strm { target } => {
                    update_digest(&mut digest, "strm");
                    update_digest(&mut digest, target);
                }
            }
        }
        if let Some(path) = &item.image_path {
            update_digest(&mut digest, path);
            update_file_metadata(&mut digest, Path::new(path))?;
        }
        if let Ok(encoded) = serde_json::to_vec(&(
            &item.runtime_ticks,
            &item.media_streams,
            &item.metadata,
            &item.parent_id,
        )) {
            digest.update(encoded);
        }
        for stream in &item.media_streams {
            if let Some(path) = &stream.external_path {
                update_file_metadata(&mut digest, Path::new(path))?;
            }
        }
    }
    cache.finish_scan();
    Ok(CatalogSnapshot {
        revision: hex::encode(digest.finalize()),
        items,
    })
}

fn read_probe_sidecar(path: &Path) -> Option<ProbeData> {
    let path = path.with_extension("ffprobe.json");
    let meta = fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > 1024 * 1024 {
        return None;
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= 1024 * 1024)
        .then(|| crate::probe::parse_probe(&bytes))
        .flatten()
}

fn item_id(library: &str, relative: &str) -> String {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("emby-bridge:{library}/{relative}").as_bytes(),
    )
    .to_string()
}

fn file_date(metadata: &fs::Metadata) -> Option<String> {
    // Modification time survives copies more reliably than filesystem birth time.
    metadata.modified().ok().map(|time| {
        chrono::DateTime::<chrono::Utc>::from(time)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    })
}

fn read_strm(path: &Path) -> io::Result<Option<String>> {
    if fs::metadata(path)?.len() > 8192 {
        return Ok(None);
    }
    let mut content = String::new();
    match fs::File::open(path)?.read_to_string(&mut content) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::InvalidData => return Ok(None),
        Err(error) => return Err(error),
    }
    let Some(candidate) = content
        .lines()
        .map(|line| line.trim().trim_start_matches('\u{feff}'))
        .find(|line| !line.is_empty())
    else {
        return Ok(None);
    };
    let Ok(url) = Url::parse(candidate) else {
        return Ok(None);
    };
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Ok(None);
    }
    Ok(Some(candidate.to_owned()))
}

fn folder_image(directory: &Path) -> Option<PathBuf> {
    ["folder.jpg", "poster.jpg", "folder.png", "poster.png"]
        .iter()
        .map(|name| directory.join(name))
        .find(|path| regular_file(path))
}

fn video_image(video: &Path) -> Option<PathBuf> {
    let stem = video.file_stem()?.to_str()?;
    for extension in ["jpg", "jpeg", "png", "webp"] {
        let candidate = video.with_file_name(format!("{stem}.{extension}"));
        if regular_file(&candidate) {
            return Some(candidate);
        }
    }
    video.parent().and_then(folder_image)
}

fn regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_file())
        .unwrap_or(false)
}

fn path_string(path: PathBuf) -> String {
    path.to_string_lossy().into_owned()
}

fn update_digest(digest: &mut Sha256, value: &str) {
    digest.update((value.len() as u64).to_le_bytes());
    digest.update(value.as_bytes());
}

fn update_file_metadata(digest: &mut Sha256, path: &Path) -> io::Result<()> {
    let metadata = fs::metadata(path)?;
    digest.update(metadata.len().to_le_bytes());
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok());
    if let Some(modified) = modified {
        digest.update(modified.as_secs().to_le_bytes());
        digest.update(modified.subsec_nanos().to_le_bytes());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn scans_files_and_strm_with_stable_ids_and_parents() {
        let temp = tempdir().unwrap();
        let films = temp.path().join("Films");
        fs::create_dir(&films).unwrap();
        fs::write(films.join("Movie.mp4"), b"video").unwrap();
        fs::write(films.join("Movie.jpg"), b"poster").unwrap();
        fs::write(
            films.join("Remote.strm"),
            "\nhttps://example.test/remote.mkv?token=temporary\n",
        )
        .unwrap();
        fs::write(films.join("Broken.strm"), "not a URL").unwrap();
        let roots = [LibraryRoot {
            name: "Movies".to_owned(),
            path: temp.path().to_string_lossy().into_owned(),
        }];

        let first = scan_roots(&roots).unwrap();
        let second = scan_roots(&roots).unwrap();
        assert_eq!(first.revision, second.revision);
        assert_eq!(first.items.len(), 4);
        let folder = first
            .items
            .iter()
            .find(|item| item.name == "Films")
            .unwrap();
        let local = first
            .items
            .iter()
            .find(|item| item.name == "Movie")
            .unwrap();
        assert_eq!(local.parent_id.as_deref(), Some(folder.id.as_str()));
        assert!(local.image_path.is_some());
        assert!(matches!(local.source, Some(MediaSource::File { .. })));
        let remote = first
            .items
            .iter()
            .find(|item| item.name == "Remote")
            .unwrap();
        assert_eq!(remote.container.as_deref(), Some("mkv"));
    }

    #[test]
    fn changed_file_updates_revision_not_logical_id() {
        let temp = tempdir().unwrap();
        let file = temp.path().join("Movie.mp4");
        fs::write(&file, b"first").unwrap();
        let roots = [LibraryRoot {
            name: "Movies".to_owned(),
            path: temp.path().to_string_lossy().into_owned(),
        }];
        let first = scan_roots(&roots).unwrap();
        fs::write(&file, b"longer content").unwrap();
        let second = scan_roots(&roots).unwrap();
        assert_ne!(first.revision, second.revision);
        assert_eq!(first.items[1].id, second.items[1].id);
    }

    #[test]
    fn scans_nfo_and_groups_flat_and_nested_episodes() {
        let temp = tempdir().unwrap();
        let show = temp.path().join("Show");
        let season = show.join("Season 02");
        fs::create_dir_all(&season).unwrap();
        fs::write(show.join("tvshow.nfo"),
            "<tvshow><title>Example &amp; Friends</title><plot>Plot</plot><genre>Drama</genre><year>2024</year></tvshow>").unwrap();
        fs::write(show.join("Show.S01E01.strm"), "https://example.test/1.mp4").unwrap();
        fs::write(show.join("Show.S01E01.nfo"),
            "<episodedetails><title>Pilot</title><season>1</season><episode>1</episode><runtime>42</runtime><aired>2024-01-02</aired></episodedetails>").unwrap();
        fs::write(show.join("Show.S01E02.strm"), "https://example.test/2.mp4").unwrap();
        fs::write(
            season.join("Show.S02E01.strm"),
            "https://example.test/3.mp4",
        )
        .unwrap();
        fs::write(
            temp.path().join("Film.strm"),
            "https://example.test/movie.mp4",
        )
        .unwrap();
        fs::write(temp.path().join("Film.nfo"),
            "<movie><title>Film</title><genre>Comedy</genre><tag>Test</tag><year>2025</year><rating>8.2</rating><uniqueid type=\"tmdb\">123</uniqueid></movie>").unwrap();
        let roots = [LibraryRoot {
            name: "Media".into(),
            path: temp.path().to_string_lossy().into_owned(),
        }];
        let mut cache = ProbeCache::without_probe();
        let first = scan_roots_with_cache(&roots, &mut cache).unwrap();
        let series = first
            .items
            .iter()
            .find(|item| item.kind == ItemKind::Series)
            .unwrap();
        assert_eq!(series.name, "Example & Friends");
        let pilot = first
            .items
            .iter()
            .find(|item| item.name == "Pilot")
            .unwrap();
        assert_eq!(pilot.runtime_ticks, Some(25_200_000_000));
        assert_eq!(
            pilot.metadata.series_id.as_deref(),
            Some(series.id.as_str())
        );
        assert_eq!(
            pilot.metadata.premiere_date.as_deref(),
            Some("2024-01-02T00:00:00Z")
        );
        let mut query = crate::query::Parameters::from([
            ("ParentId".into(), pilot.metadata.season_id.clone().unwrap()),
            ("Recursive".into(), "true".into()),
        ]);
        assert_eq!(
            crate::query::select(first.items.clone(), &Default::default(), &query).len(),
            2
        );
        crate::query::put(&mut query, "ParentId", series.id.clone());
        assert_eq!(
            crate::query::select(first.items.clone(), &Default::default(), &query).len(),
            5
        );
        let film = first
            .items
            .iter()
            .find(|item| item.kind == ItemKind::Movie)
            .unwrap();
        assert_eq!(
            film.metadata.provider_ids.get("Tmdb").map(String::as_str),
            Some("123")
        );
        assert_eq!(film.metadata.genres, ["Comedy"]);
        fs::write(
            show.join("tvshow.nfo"),
            "<tvshow><title>Renamed</title></tvshow>",
        )
        .unwrap();
        let second = scan_roots_with_cache(&roots, &mut cache).unwrap();
        assert_ne!(first.revision, second.revision);
        let renamed = second
            .items
            .iter()
            .find(|item| item.kind == ItemKind::Series)
            .unwrap();
        assert_eq!(series.id, renamed.id);
        let next_pilot = second
            .items
            .iter()
            .find(|item| item.name == "Pilot")
            .unwrap();
        assert_eq!(pilot.id, next_pilot.id);
        assert_eq!(pilot.metadata.season_id, next_pilot.metadata.season_id);
    }
}
