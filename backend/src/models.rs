use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Clone, Debug, FromRow)]
pub struct MusicRecord {
    pub id: String,
    pub title: String,
    pub filename: String,
    pub content_type: String,
    pub object_key: String,
    pub audio_object_key: Option<String>,
    pub audio_status: String,
    pub audio_error: Option<String>,
    pub midi_object_key: Option<String>,
    pub midi_status: String,
    pub midi_error: Option<String>,
    pub musicxml_object_key: Option<String>,
    pub musicxml_status: String,
    pub musicxml_error: Option<String>,
    pub stems_status: String,
    pub stems_error: Option<String>,
    pub public_token: String,
    pub public_id: Option<String>,
    pub quality_profile: String,
    pub created_at: String,
}

#[derive(Clone, Debug, FromRow)]
pub struct StemRecord {
    pub id: i64,
    pub music_id: String,
    pub track_index: i64,
    pub track_name: String,
    pub instrument_name: String,
    pub storage_key: String,
    pub drum_map_json: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DrumMapEntry {
    pub pitch: u8,
    pub name: String,
    pub head: Option<String>,
    pub line: Option<i8>,
    pub voice: Option<u8>,
    pub stem: Option<i8>,
    pub shortcut: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub ok: bool,
}

#[derive(Debug, Deserialize)]
pub struct UpdateMusicRequest {
    pub public_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ExportMixerGainsRequest {
    pub tracks: Vec<ExportMixerTrackRequest>,
}

#[derive(Debug, Deserialize)]
pub struct ExportMixerTrackRequest {
    pub track_index: usize,
    pub volume_multiplier: f64,
    #[serde(default)]
    pub muted: bool,
}

#[derive(Debug, Serialize)]
pub struct StemInfo {
    pub track_index: i64,
    pub track_name: String,
    pub instrument_name: String,
    pub full_stem_url: String,
    pub duration_seconds: f64,
    pub drum_map: Option<Vec<DrumMapEntry>>,
}

#[derive(Debug, Serialize)]
pub struct AdminMusicResponse {
    pub id: String,
    pub title: String,
    pub filename: String,
    pub content_type: String,
    pub audio_status: String,
    pub audio_error: Option<String>,
    pub midi_status: String,
    pub midi_error: Option<String>,
    pub musicxml_status: String,
    pub musicxml_error: Option<String>,
    pub stems_status: String,
    pub stems_error: Option<String>,
    pub public_token: String,
    pub public_id: Option<String>,
    pub public_url: String,
    pub public_id_url: Option<String>,
    pub download_url: String,
    pub midi_download_url: Option<String>,
    pub quality_profile: String,
    pub created_at: String,
    pub stems_total_bytes: i64,
}

#[derive(Debug, Serialize)]
pub struct PublicMusicResponse {
    pub title: String,
    pub filename: String,
    pub audio_status: String,
    pub audio_error: Option<String>,
    pub can_stream_audio: bool,
    pub audio_stream_url: Option<String>,
    pub midi_status: String,
    pub midi_error: Option<String>,
    pub midi_download_url: Option<String>,
    pub musicxml_url: Option<String>,
    pub stems_status: String,
    pub stems_error: Option<String>,
    pub download_url: String,
    pub created_at: String,
}
