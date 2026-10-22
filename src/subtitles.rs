use std::{fs, io, path::Path, process::Stdio, time::Duration};

use axum::{
    body::Body,
    http::{
        HeaderMap, HeaderValue, Method, Response, StatusCode,
        header::{CONTENT_LENGTH, CONTENT_TYPE},
    },
};
use tokio::{io::AsyncReadExt, process::Command, sync::Semaphore};

use crate::{
    model::{MediaItem, MediaSource, MediaStream},
    streaming::{local_file, response},
};

const MAX_SUBTITLE_BYTES: u64 = 16 * 1024 * 1024;
static CONVERSIONS: Semaphore = Semaphore::const_new(2);

pub fn discover(item: &mut MediaItem, video: &Path, files: &[fs::DirEntry]) -> io::Result<()> {
    let Some(stem) = video.file_stem().and_then(|name| name.to_str()) else {
        return Ok(());
    };
    let mut index = item
        .media_streams
        .iter()
        .map(|stream| stream.index)
        .max()
        .unwrap_or(-1)
        + 1;
    for file in files {
        if !file.file_type()?.is_file() {
            continue;
        }
        let path = file.path();
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !matches!(extension.as_str(), "srt" | "ass" | "ssa" | "vtt")
            || file.metadata()?.len() > MAX_SUBTITLE_BYTES
        {
            continue;
        }
        let subtitle_stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        let suffix = if subtitle_stem == stem {
            ""
        } else if let Some(suffix) = subtitle_stem.strip_prefix(&format!("{stem}.")) {
            suffix
        } else {
            continue;
        };
        let labels: Vec<_> = suffix
            .split('.')
            .filter(|label| !label.is_empty())
            .collect();
        let language = labels
            .iter()
            .find(|label| {
                !matches!(
                    label.to_ascii_lowercase().as_str(),
                    "default" | "forced" | "sdh" | "cc"
                )
            })
            .map(|value| (*value).to_owned());
        item.media_streams.push(MediaStream {
            index,
            stream_type: "Subtitle".into(),
            codec: Some(extension),
            language,
            title: Some(if suffix.is_empty() {
                "External".into()
            } else {
                suffix.into()
            }),
            is_default: labels
                .iter()
                .any(|label| label.eq_ignore_ascii_case("default")),
            is_forced: labels
                .iter()
                .any(|label| label.eq_ignore_ascii_case("forced")),
            is_external: true,
            external_path: Some(path.to_string_lossy().into_owned()),
            ..Default::default()
        });
        index += 1;
    }
    Ok(())
}

pub fn text_codec(codec: Option<&str>) -> bool {
    matches!(
        codec,
        Some("srt" | "subrip" | "ass" | "ssa" | "vtt" | "webvtt" | "mov_text" | "text")
    )
}

pub async fn serve(
    item: &MediaItem,
    stream: &MediaStream,
    path: &Path,
    format: &str,
    start: i64,
    method: &Method,
    headers: &HeaderMap,
) -> Response<Body> {
    let mime = match format {
        "srt" => "application/x-subrip; charset=utf-8",
        "vtt" => "text/vtt; charset=utf-8",
        "ass" => "text/x-ssa; charset=utf-8",
        _ => return response(StatusCode::UNSUPPORTED_MEDIA_TYPE, Body::empty()),
    };
    if !text_codec(stream.codec.as_deref()) || start < 0 || (start > 0 && format == "ass") {
        return response(StatusCode::UNSUPPORTED_MEDIA_TYPE, Body::empty());
    }
    if stream.is_external && stream.codec.as_deref() == Some(format) && start == 0 {
        let mut result = local_file(path, method, headers).await;
        result
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static(mime));
        return result;
    }
    if !stream.is_external && !matches!(item.source, Some(MediaSource::File { .. })) {
        return response(StatusCode::UNSUPPORTED_MEDIA_TYPE, Body::empty());
    }
    let Ok(_permit) = CONVERSIONS.try_acquire() else {
        return response(StatusCode::SERVICE_UNAVAILABLE, Body::empty());
    };
    let binary = std::env::var_os("EMBY_FFMPEG").unwrap_or_else(|| "ffmpeg".into());
    let mut command = Command::new(binary);
    command
        .args([
            "-nostdin",
            "-v",
            "error",
            "-protocol_whitelist",
            "file,pipe",
            "-i",
        ])
        .arg(path);
    command.args([
        "-map",
        &format!("0:{}", if stream.is_external { 0 } else { stream.index }),
    ]);
    if start > 0 {
        command.args(["-ss", &format!("{:.7}", start as f64 / 10_000_000.0)]);
    }
    command
        .args([
            "-f",
            if format == "vtt" { "webvtt" } else { format },
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let Ok(mut child) = command.spawn() else {
        return response(StatusCode::SERVICE_UNAVAILABLE, Body::empty());
    };
    let mut stdout = child
        .stdout
        .take()
        .expect("piped stdout")
        .take(MAX_SUBTITLE_BYTES + 1);
    let mut bytes = Vec::new();
    let output = tokio::time::timeout(Duration::from_secs(15), async {
        let (read, status) = tokio::join!(stdout.read_to_end(&mut bytes), child.wait());
        read.ok()?;
        status.ok().filter(|status| status.success())
    })
    .await;
    if !matches!(output, Ok(Some(_))) || bytes.len() as u64 > MAX_SUBTITLE_BYTES {
        let _ = child.kill().await;
        let _ = child.wait().await;
        return response(StatusCode::BAD_GATEWAY, Body::empty());
    }
    let length = bytes.len();
    let mut result = response(
        StatusCode::OK,
        if method == Method::HEAD {
            Body::empty()
        } else {
            Body::from(bytes)
        },
    );
    result
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(mime));
    result.headers_mut().insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string()).unwrap(),
    );
    result
}
