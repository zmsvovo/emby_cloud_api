use std::path::PathBuf;

use url::Url;

use crate::model::{MediaItem, MediaSource};

pub enum ResolvedSource {
    Local(PathBuf),
    Redirect(Url),
}

/// A node can replace this policy to resolve provider-specific STRM entries.
pub trait SourceResolver: Send + Sync {
    fn resolve(&self, item: &MediaItem) -> Option<ResolvedSource>;
}

pub struct DefaultSourceResolver;

impl SourceResolver for DefaultSourceResolver {
    fn resolve(&self, item: &MediaItem) -> Option<ResolvedSource> {
        match item.source.as_ref()? {
            MediaSource::File { path } => Some(ResolvedSource::Local(PathBuf::from(path))),
            MediaSource::Strm { target } => {
                let url = Url::parse(target).ok()?;
                (matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none())
                .then_some(ResolvedSource::Redirect(url))
            }
        }
    }
}
