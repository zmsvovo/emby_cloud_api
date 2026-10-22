use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LibraryRoot {
    pub name: String,
    pub path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MediaSource {
    File { path: String },
    Strm { target: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Library,
    Folder,
    Video,
    Movie,
    Series,
    Season,
    Episode,
}

impl ItemKind {
    pub fn is_video(&self) -> bool {
        matches!(self, Self::Video | Self::Movie | Self::Episode)
    }

    pub fn emby_type(&self) -> &'static str {
        match self {
            Self::Library => "CollectionFolder",
            Self::Folder => "Folder",
            Self::Video => "Video",
            Self::Movie => "Movie",
            Self::Series => "Series",
            Self::Season => "Season",
            Self::Episode => "Episode",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ItemMetadata {
    pub version_group: Option<String>,
    pub version_name: Option<String>,
    pub collection_type: Option<String>,
    pub original_title: Option<String>,
    pub sort_title: Option<String>,
    pub overview: Option<String>,
    pub year: Option<u32>,
    pub premiere_date: Option<String>,
    pub community_rating: Option<f32>,
    pub official_rating: Option<String>,
    pub genres: Vec<String>,
    pub tags: Vec<String>,
    pub provider_ids: std::collections::BTreeMap<String, String>,
    pub index_number: Option<u32>,
    pub parent_index_number: Option<u32>,
    pub series_id: Option<String>,
    pub series_name: Option<String>,
    pub season_id: Option<String>,
    pub season_name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MediaItem {
    pub id: String,
    pub parent_id: Option<String>,
    pub name: String,
    pub kind: ItemKind,
    pub library_name: String,
    pub relative_path: String,
    pub source: Option<MediaSource>,
    pub container: Option<String>,
    pub size: Option<u64>,
    pub image_path: Option<String>,
    #[serde(default)]
    pub date_created: Option<String>,
    #[serde(default)]
    pub runtime_ticks: Option<i64>,
    #[serde(default)]
    pub media_streams: Vec<MediaStream>,
    #[serde(default)]
    pub metadata: ItemMetadata,
    #[serde(default)]
    pub versions: Vec<MediaItem>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct MediaStream {
    pub index: i32,
    pub stream_type: String,
    pub codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub channels: Option<u32>,
    pub sample_rate: Option<u32>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub is_default: bool,
    pub is_forced: bool,
    pub is_external: bool,
    pub external_path: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CatalogSnapshot {
    pub revision: String,
    pub items: Vec<MediaItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeAdvert {
    pub id: String,
    pub base_url: String,
    pub weight: u32,
    pub catalog: CatalogSnapshot,
}
