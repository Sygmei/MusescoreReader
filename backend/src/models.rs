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

#[derive(Debug, Serialize)]
pub struct StemInfo {
    pub track_index: i64,
    pub track_name: String,
    pub instrument_name: String,
    pub chunk_url_template: String,
    pub chunk_count: i64,
    pub chunk_duration_seconds: f64,
    pub duration_seconds: f64,
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
