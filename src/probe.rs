use std::{
    collections::HashMap,
    ffi::OsString,
    fs::Metadata,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime},
};

use serde::Deserialize;
use serde_json::Value;

use crate::model::MediaStream;

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_OUTPUT: u64 = 1024 * 1024;

#[derive(Clone, Default)]
pub struct ProbeData {
    pub runtime_ticks: Option<i64>,
    pub media_streams: Vec<MediaStream>,
}

struct CachedProbe {
    size: u64,
    modified: Option<SystemTime>,
    data: ProbeData,
    generation: u64,
    retry_after: Option<Instant>,
}

pub struct ProbeCache {
    entries: HashMap<PathBuf, CachedProbe>,
    binary: OsString,
    available: bool,
    generation: u64,
}

impl Default for ProbeCache {
    fn default() -> Self {
        let binary = std::env::var_os("EMBY_FFPROBE").unwrap_or_else(|| OsString::from("ffprobe"));
        let available = !binary.is_empty();
        Self {
            entries: HashMap::new(),
            binary,
            available,
            generation: 0,
        }
    }
}

impl ProbeCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn without_probe() -> Self {
        let mut cache = Self::new();
        cache.available = false;
        cache
    }

    pub(crate) fn begin_scan(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    pub(crate) fn finish_scan(&mut self) {
        self.entries
            .retain(|_, entry| entry.generation == self.generation);
    }

    pub(crate) fn get(&mut self, path: &Path, metadata: &Metadata) -> ProbeData {
        let modified = metadata.modified().ok();
        if let Some(cached) = self.entries.get_mut(path) {
            if cached.size == metadata.len()
                && cached.modified == modified
                && cached
                    .retry_after
                    .is_none_or(|deadline| Instant::now() < deadline)
            {
                cached.generation = self.generation;
                return cached.data.clone();
            }
        }

        let mut retry_after = None;
        let data = if self.available {
            match run_ffprobe(&self.binary, path) {
                Ok(data) => data,
                Err(ProbeError::Missing) => {
                    self.available = false;
                    ProbeData::default()
                }
                Err(ProbeError::Failed(reason)) => {
                    tracing::debug!(path = %path.display(), %reason, "ffprobe unavailable for file");
                    retry_after = Some(Instant::now() + Duration::from_secs(60));
                    ProbeData::default()
                }
            }
        } else {
            ProbeData::default()
        };
        self.entries.insert(
            path.to_owned(),
            CachedProbe {
                size: metadata.len(),
                modified,
                data: data.clone(),
                generation: self.generation,
                retry_after,
            },
        );
        data
    }
}

enum ProbeError {
    Missing,
    Failed(String),
}

fn run_ffprobe(binary: &OsString, path: &Path) -> Result<ProbeData, ProbeError> {
    // Only paths discovered under configured local roots reach this process.
    let mut child = Command::new(binary)
        .args([
            "-v",
            "error",
            "-protocol_whitelist",
            "file,pipe",
            "-show_format",
            "-show_streams",
            "-of",
            "json",
            "-i",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ProbeError::Missing
            } else {
                ProbeError::Failed(error.to_string())
            }
        })?;

    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ProbeError::Failed("missing stdout pipe".into()));
    };
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        stdout
            .take(MAX_OUTPUT + 1)
            .read_to_end(&mut output)
            .map(|_| output)
    });

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if start.elapsed() < PROBE_TIMEOUT => {
                thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ProbeError::Failed(error.to_string()));
            }
        }
    };
    let output = reader
        .join()
        .map_err(|_| ProbeError::Failed("output reader panicked".into()))?
        .map_err(|error| ProbeError::Failed(error.to_string()))?;
    if !status || output.len() as u64 > MAX_OUTPUT {
        return Err(ProbeError::Failed(
            "process failed, timed out, or output too large".into(),
        ));
    }
    parse_probe(&output).ok_or_else(|| ProbeError::Failed("invalid ffprobe JSON".into()))
}

#[derive(Deserialize)]
struct ProbeOutput {
    format: Option<ProbeFormat>,
    #[serde(default)]
    streams: Vec<ProbeStream>,
}

#[derive(Deserialize)]
struct ProbeFormat {
    duration: Option<Value>,
}

#[derive(Deserialize)]
struct ProbeStream {
    index: i32,
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    channels: Option<u32>,
    sample_rate: Option<Value>,
    duration: Option<Value>,
    #[serde(default)]
    tags: HashMap<String, String>,
    #[serde(default)]
    disposition: HashMap<String, i32>,
}

pub(crate) fn parse_probe(json: &[u8]) -> Option<ProbeData> {
    let output: ProbeOutput = serde_json::from_slice(json).ok()?;
    let duration = output
        .format
        .as_ref()
        .and_then(|format| format.duration.as_ref())
        .and_then(number)
        .or_else(|| {
            output
                .streams
                .iter()
                .filter_map(|stream| stream.duration.as_ref().and_then(number))
                .reduce(f64::max)
        });
    let runtime_ticks = duration
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .map(|seconds| (seconds * 10_000_000.0).round().min(i64::MAX as f64) as i64);
    let media_streams = output
        .streams
        .into_iter()
        .filter(|stream| (0..=65535).contains(&stream.index))
        .filter_map(|stream| {
            let stream_type = match stream.codec_type.as_deref()? {
                "video" => "Video",
                "audio" => "Audio",
                "subtitle" => "Subtitle",
                _ => return None,
            };
            Some(MediaStream {
                index: stream.index,
                stream_type: stream_type.to_owned(),
                codec: stream.codec_name,
                width: stream.width,
                height: stream.height,
                channels: stream.channels,
                sample_rate: stream
                    .sample_rate
                    .as_ref()
                    .and_then(number)
                    .and_then(|rate| {
                        (rate.is_finite() && rate >= 0.0 && rate <= u32::MAX as f64)
                            .then_some(rate as u32)
                    }),
                language: stream.tags.get("language").cloned(),
                title: stream.tags.get("title").cloned(),
                is_default: stream.disposition.get("default") == Some(&1),
                is_forced: stream.disposition.get("forced") == Some(&1),
                ..Default::default()
            })
        })
        .collect();
    Some(ProbeData {
        runtime_ticks,
        media_streams,
    })
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_failed_probe_is_not_reused_forever() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let metadata = file.as_file().metadata().unwrap();
        let mut cache = ProbeCache::without_probe();
        cache.entries.insert(
            file.path().to_owned(),
            CachedProbe {
                size: metadata.len(),
                modified: metadata.modified().ok(),
                data: ProbeData {
                    runtime_ticks: Some(123),
                    media_streams: Vec::new(),
                },
                generation: 0,
                retry_after: Some(Instant::now() - Duration::from_secs(1)),
            },
        );
        assert!(cache.get(file.path(), &metadata).runtime_ticks.is_none());
    }

    #[test]
    fn parses_duration_and_video_audio_streams() {
        let probe = parse_probe(
            br#"{"format":{"duration":"12.5"},"streams":[
                {"index":0,"codec_type":"video","codec_name":"h264","width":1920,"height":1080},
                {"index":1,"codec_type":"audio","codec_name":"aac","channels":2,"sample_rate":"48000","tags":{"language":"zho","title":"Mandarin"},"disposition":{"default":1}},
                {"index":2,"codec_type":"subtitle","codec_name":"ass","tags":{"language":"eng"},"disposition":{"forced":1}}
            ]}"#,
        )
        .unwrap();
        assert_eq!(probe.runtime_ticks, Some(125_000_000));
        assert_eq!(probe.media_streams.len(), 3);
        assert_eq!(probe.media_streams[0].width, Some(1920));
        assert_eq!(probe.media_streams[1].sample_rate, Some(48000));
        assert_eq!(probe.media_streams[1].language.as_deref(), Some("zho"));
        assert!(probe.media_streams[1].is_default);
        assert!(probe.media_streams[2].is_forced);
    }
}
