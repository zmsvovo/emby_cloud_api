use std::path::Path;

use axum::{
    body::Body,
    http::{
        HeaderMap, HeaderValue, Method, Response, StatusCode,
        header::{
            ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, IF_RANGE, RANGE,
        },
    },
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

pub async fn local_file(path: &Path, method: &Method, headers: &HeaderMap) -> Response<Body> {
    let Ok(metadata) = tokio::fs::metadata(path).await else {
        return response(StatusCode::NOT_FOUND, Body::empty());
    };
    if !metadata.is_file() {
        return response(StatusCode::NOT_FOUND, Body::empty());
    }
    let size = metadata.len();
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |value| value.as_nanos());
    let etag = format!("\"{size:x}-{modified:x}\"");
    let range = headers
        .get(IF_RANGE)
        .and_then(|value| value.to_str().ok())
        .filter(|value| *value != etag)
        .map(|_| None)
        .unwrap_or_else(|| headers.get(RANGE).and_then(|header| header.to_str().ok()));
    let (start, end) = match range {
        Some(value) => match parse_range(value, size) {
            Some(range) => range,
            None => {
                let mut result = response(StatusCode::RANGE_NOT_SATISFIABLE, Body::empty());
                result.headers_mut().insert(
                    CONTENT_RANGE,
                    HeaderValue::from_str(&format!("bytes */{size}")).expect("range header"),
                );
                return result;
            }
        },
        None => (0, size.saturating_sub(1)),
    };
    let length = if size == 0 { 0 } else { end - start + 1 };
    let status = if range.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let body = if method == Method::HEAD || length == 0 {
        Body::empty()
    } else {
        let Ok(mut file) = tokio::fs::File::open(path).await else {
            return response(StatusCode::NOT_FOUND, Body::empty());
        };
        if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
            return response(StatusCode::INTERNAL_SERVER_ERROR, Body::empty());
        }
        Body::from_stream(ReaderStream::new(file.take(length)))
    };
    let mut result = response(status, body);
    let content_type = mime_guess::from_path(path).first_or_octet_stream();
    if let Ok(value) = HeaderValue::from_str(content_type.as_ref()) {
        result.headers_mut().insert(CONTENT_TYPE, value);
    }
    result
        .headers_mut()
        .insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    result
        .headers_mut()
        .insert(ETAG, HeaderValue::from_str(&etag).expect("etag header"));
    result.headers_mut().insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string()).expect("length header"),
    );
    if range.is_some() {
        result.headers_mut().insert(
            CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{size}")).expect("range header"),
        );
    }
    result
}

pub fn response(status: StatusCode, body: Body) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(body)
        .expect("HTTP response")
}

// This service supports one byte range, including suffix and open-ended ranges.
fn parse_range(value: &str, size: u64) -> Option<(u64, u64)> {
    let specification = value.strip_prefix("bytes=")?;
    if size == 0 || specification.contains(',') {
        return None;
    }
    let (start, end) = specification.split_once('-')?;
    if start.is_empty() {
        let suffix: u64 = end.parse().ok()?;
        if suffix == 0 {
            return None;
        }
        return Some((size.saturating_sub(suffix), size - 1));
    }
    let start: u64 = start.parse().ok()?;
    if start >= size {
        return None;
    }
    let end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<u64>().ok()?.min(size - 1)
    };
    (start <= end).then_some((start, end))
}

#[cfg(test)]
mod tests {
    use super::parse_range;

    #[test]
    fn handles_basic_and_unsatisfiable_ranges() {
        assert_eq!(parse_range("bytes=5-9", 20), Some((5, 9)));
        assert_eq!(parse_range("bytes=5-", 20), Some((5, 19)));
        assert_eq!(parse_range("bytes=-5", 20), Some((15, 19)));
        assert_eq!(parse_range("bytes=20-", 20), None);
        assert_eq!(parse_range("bytes=0-1,5-6", 20), None);
    }
}
