mod audio;
mod config;
mod models;
mod storage;

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::multipart::MultipartError,
    extract::{DefaultBodyLimit, Multipart, Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
};
use bytes::Bytes;
use config::{AppConfig, StorageConfig};
use models::{
    AdminMusicResponse, LoginRequest, LoginResponse, MusicRecord, PublicMusicResponse, StemInfo,
    StemRecord, UpdateMusicRequest,
};
use rand::{Rng, distr::Alphanumeric};
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::str::FromStr;
use std::{net::SocketAddr, path::PathBuf};
use storage::Storage;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::process::Command;
use tower_http::{
    cors::{Any, CorsLayer},
    services::{ServeDir, ServeFile},
};
use tracing::{info, warn};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    config: AppConfig,
    db_rw: PgPool,
    db_ro: PgPool,
    storage: Storage,
}

#[derive(Debug)]
struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }
}

impl From<anyhow::Error> for AppError {
    fn from(error: anyhow::Error) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
    }
}

impl From<sqlx::Error> for AppError {
    fn from(error: sqlx::Error) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
    }
}

impl From<std::io::Error> for AppError {
    fn from(error: std::io::Error) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
    }
}

impl From<MultipartError> for AppError {
    fn from(error: MultipartError) -> Self {
        Self::new(StatusCode::BAD_REQUEST, error.to_string())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "fumen_backend=info,tower_http=info".to_owned()),
        )
        .init();

    let config = AppConfig::from_env()?;
    match &config.storage {
        StorageConfig::Local { root } => {
            info!("using local storage at {}", root.display());
        }
        StorageConfig::S3(s3) => {
            info!("using s3 storage bucket {}", s3.bucket);
        }
    }

    let db_admin = open_database_pool(&config.database_url_admin, 1, "admin").await?;
    ensure_schema(&db_admin).await?;
    let db_rw = open_database_pool(&config.database_url, 5, "read-write").await?;
    let db_ro = open_database_pool(&config.database_url_read_only, 5, "read-only").await?;
    let storage = Storage::new(&config).await?;

    let state = AppState {
        config,
        db_rw,
        db_ro,
        storage,
    };
    let api_routes = Router::new()
        .route("/health", get(health))
        .route("/admin/login", post(admin_login))
        .route(
            "/admin/musics",
            get(admin_list_musics).post(admin_upload_music),
        )
        .route("/admin/musics/{id}", patch(admin_update_music))
        .route("/admin/musics/{id}/retry", post(admin_retry_render))
        .route("/public/{access_key}", get(public_music))
        .route("/public/{access_key}/audio", get(public_music_audio))
        .route("/public/{access_key}/midi", get(public_music_midi))
        .route("/public/{access_key}/musicxml", get(public_music_musicxml))
        .route("/public/{access_key}/stems", get(public_music_stems))
        .route(
            "/public/{access_key}/stems/{track_index}",
            get(public_music_stem_audio),
        )
        .route("/public/{access_key}/download", get(public_music_download))
        .with_state(state.clone());

    let mut app = Router::new()
        .nest("/api", api_routes)
        .layer(DefaultBodyLimit::max(50 * 1024 * 1024))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_headers(Any)
                .allow_methods(Any),
        );

    let frontend_dist = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../frontend/dist");
    if frontend_dist.exists() {
        app = app.fallback_service(
            ServeDir::new(&frontend_dist)
                .not_found_service(ServeFile::new(frontend_dist.join("index.html"))),
        );
    } else {
        app = app.route("/", get(root_message));
    }

    let address: SocketAddr = state
        .config
        .bind_address
        .parse()
        .with_context(|| format!("invalid BIND_ADDRESS '{}'", state.config.bind_address))?;

    info!("listening on http://{}", address);
    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn open_database_pool(url: &str, max_connections: u32, role: &str) -> Result<PgPool> {
    let options = PgConnectOptions::from_str(url)
        .with_context(|| format!("invalid PostgreSQL connection string for {role} pool"))?
        .statement_cache_capacity(0);

    Ok(PgPoolOptions::new()
        .max_connections(max_connections)
        .connect_with(options)
        .await?)
}

async fn ensure_schema(db: &PgPool) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS musics (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            filename TEXT NOT NULL,
            content_type TEXT NOT NULL,
            object_key TEXT NOT NULL,
            audio_object_key TEXT,
            audio_status TEXT NOT NULL DEFAULT 'unavailable',
            audio_error TEXT,
            midi_object_key TEXT,
            midi_status TEXT NOT NULL DEFAULT 'unavailable',
            midi_error TEXT,
            stems_status TEXT NOT NULL DEFAULT 'unavailable',
            stems_error TEXT,
            public_token TEXT NOT NULL UNIQUE,
            public_id TEXT UNIQUE,
            quality_profile TEXT NOT NULL DEFAULT 'standard',
            created_at TEXT NOT NULL
        )
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS stems (
            id BIGSERIAL PRIMARY KEY,
            music_id TEXT NOT NULL REFERENCES musics(id),
            track_index BIGINT NOT NULL,
            track_name TEXT NOT NULL,
            instrument_name TEXT NOT NULL,
            storage_key TEXT NOT NULL,
            size_bytes BIGINT NOT NULL DEFAULT 0,
            drum_map_json TEXT
        )
        "#,
    )
    .execute(db)
    .await?;

    ensure_music_column(db, "audio_object_key", "TEXT").await?;
    ensure_music_column(db, "audio_status", "TEXT NOT NULL DEFAULT 'unavailable'").await?;
    ensure_music_column(db, "audio_error", "TEXT").await?;
    ensure_music_column(db, "midi_object_key", "TEXT").await?;
    ensure_music_column(db, "midi_status", "TEXT NOT NULL DEFAULT 'unavailable'").await?;
    ensure_music_column(db, "midi_error", "TEXT").await?;
    ensure_music_column(db, "stems_status", "TEXT NOT NULL DEFAULT 'unavailable'").await?;
    ensure_music_column(db, "stems_error", "TEXT").await?;
    ensure_music_column(db, "musicxml_object_key", "TEXT").await?;
    ensure_music_column(db, "musicxml_status", "TEXT NOT NULL DEFAULT 'unavailable'").await?;
    ensure_music_column(db, "musicxml_error", "TEXT").await?;
    ensure_music_column(
        db,
        "quality_profile",
        &format!(
            "TEXT NOT NULL DEFAULT '{}'",
            audio::DEFAULT_STEM_QUALITY_PROFILE
        ),
    )
    .await?;
    ensure_stems_column(db, "size_bytes", "BIGINT NOT NULL DEFAULT 0").await?;
    ensure_stems_column(db, "drum_map_json", "TEXT").await?;

    Ok(())
}

async fn ensure_music_column(db: &PgPool, name: &str, definition: &str) -> Result<()> {
    let query = format!("ALTER TABLE musics ADD COLUMN IF NOT EXISTS {name} {definition}");
    sqlx::query(&query).execute(db).await?;
    Ok(())
}

async fn ensure_stems_column(db: &PgPool, name: &str, definition: &str) -> Result<()> {
    let query = format!("ALTER TABLE stems ADD COLUMN IF NOT EXISTS {name} {definition}");
    sqlx::query(&query).execute(db).await?;
    Ok(())
}

async fn root_message() -> impl IntoResponse {
    Json(serde_json::json!({
        "message": "Fumen backend is running. Build the frontend to serve it from this process."
    }))
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "ok": true }))
}

async fn admin_login(
    State(state): State<AppState>,
    Json(payload): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, AppError> {
    if payload.password != state.config.admin_password {
        return Err(AppError::unauthorized("Invalid admin password"));
    }

    Ok(Json(LoginResponse { ok: true }))
}

#[derive(sqlx::FromRow)]
struct StemsTotalRow {
    music_id: String,
    total_bytes: i64,
}

async fn fetch_stems_total(db: &PgPool, music_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(SUM(size_bytes), 0)::BIGINT FROM stems WHERE music_id = $1",
    )
    .bind(music_id)
    .fetch_one(db)
    .await
    .unwrap_or(0)
}

async fn find_public_music_record(
    state: &AppState,
    access_key: &str,
) -> Result<Option<MusicRecord>, AppError> {
    if let Some(record) = find_music_by_access_key(&state.db_ro, access_key).await? {
        return Ok(Some(record));
    }

    Ok(find_music_by_access_key(&state.db_rw, access_key).await?)
}

async fn find_public_stems(
    db_primary: &PgPool,
    db_fallback: &PgPool,
    music_id: &str,
) -> Result<Vec<StemRecord>, AppError> {
    let query = "SELECT id, music_id, track_index, track_name, instrument_name, storage_key, drum_map_json \
         FROM stems WHERE music_id = $1 ORDER BY track_index";

    let stems = sqlx::query_as::<_, StemRecord>(query)
        .bind(music_id)
        .fetch_all(db_primary)
        .await?;

    if !stems.is_empty() {
        return Ok(stems);
    }

    Ok(sqlx::query_as::<_, StemRecord>(query)
        .bind(music_id)
        .fetch_all(db_fallback)
        .await?)
}

async fn find_public_stem(
    db_primary: &PgPool,
    db_fallback: &PgPool,
    music_id: &str,
    track_index: i64,
) -> Result<Option<StemRecord>, AppError> {
    let query = "SELECT id, music_id, track_index, track_name, instrument_name, storage_key, drum_map_json \
         FROM stems WHERE music_id = $1 AND track_index = $2";

    if let Some(stem) = sqlx::query_as::<_, StemRecord>(query)
        .bind(music_id)
        .bind(track_index)
        .fetch_optional(db_primary)
        .await?
    {
        return Ok(Some(stem));
    }

    Ok(sqlx::query_as::<_, StemRecord>(query)
        .bind(music_id)
        .bind(track_index)
        .fetch_optional(db_fallback)
        .await?)
}

async fn admin_list_musics(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<AdminMusicResponse>>, AppError> {
    require_admin(&headers, &state.config)?;

    let rows = sqlx::query_as::<_, MusicRecord>(
        r#"
        SELECT id, title, filename, content_type, object_key, audio_object_key, audio_status, audio_error, midi_object_key, midi_status, midi_error, musicxml_object_key, musicxml_status, musicxml_error, stems_status, stems_error, public_token, public_id, quality_profile, created_at
        FROM musics
        ORDER BY created_at DESC
        "#,
    )
    .fetch_all(&state.db_rw)
    .await?;

    // Fetch per-music stem totals in one query
    let total_rows = sqlx::query_as::<_, StemsTotalRow>(
        "SELECT music_id, COALESCE(SUM(size_bytes), 0)::BIGINT AS total_bytes FROM stems GROUP BY music_id",
    )
    .fetch_all(&state.db_rw)
    .await?;
    let totals: std::collections::HashMap<String, i64> = total_rows
        .into_iter()
        .map(|r| (r.music_id, r.total_bytes))
        .collect();

    let items = rows
        .into_iter()
        .map(|record| {
            let total = totals.get(&record.id).copied().unwrap_or(0);
            record_to_admin_response(&state.config, &state.storage, record, total)
        })
        .collect();

    Ok(Json(items))
}

async fn admin_upload_music(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Json<AdminMusicResponse>, AppError> {
    require_admin(&headers, &state.config)?;

    let mut title: Option<String> = None;
    let mut requested_public_id: Option<String> = None;
    let mut requested_quality_profile: Option<String> = None;
    let mut upload: Option<(String, String, Bytes)> = None;

    while let Some(field) = multipart.next_field().await? {
        match field.name() {
            Some("title") => {
                title = Some(field.text().await?.trim().to_owned());
            }
            Some("public_id") => {
                requested_public_id = Some(field.text().await?.trim().to_owned());
            }
            Some("quality_profile") => {
                requested_quality_profile = Some(field.text().await?.trim().to_owned());
            }
            Some("file") => {
                let filename = field.file_name().map(ToOwned::to_owned).ok_or_else(|| {
                    AppError::bad_request("The uploaded file is missing a filename")
                })?;
                let content_type = field
                    .content_type()
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| "application/octet-stream".to_owned());
                upload = Some((filename, content_type, field.bytes().await?));
            }
            _ => {}
        }
    }

    let (filename, content_type, bytes) =
        upload.ok_or_else(|| AppError::bad_request("Please attach an .mscz file"))?;

    if !filename.to_lowercase().ends_with(".mscz") {
        return Err(AppError::bad_request("Only .mscz uploads are supported"));
    }

    let public_id = normalize_public_id(requested_public_id.as_deref())?;
    ensure_public_id_available(&state.db_rw, public_id.as_deref(), None).await?;
    let quality_profile = parse_quality_profile(requested_quality_profile.as_deref())?;

    let music_id = Uuid::new_v4().to_string();
    let public_token = generate_public_token();
    let resolved_title = title
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| filename.trim_end_matches(".mscz").to_owned());
    let safe_filename = sanitize_filename(&filename);
    let object_key = format!("scores/{music_id}/{safe_filename}");

    state
        .storage
        .upload_bytes(&object_key, bytes.clone(), &content_type)
        .await?;

    let temp_dir = tempfile::tempdir()?;
    let temp_input_path = temp_dir.path().join(&safe_filename);
    fs::write(&temp_input_path, &bytes).await?;

    // Pipeline:
    //   t=0      → MIDI export and MusicXML export run in parallel (both MuseScore passes)
    //   t=T_midi → stems render (parallel internally, reuse preview.mid)
    //   t=T_midi+T_stems → three-way parallel:
    //                      • upload MIDI
    //                      • upload MusicXML
    //                      • upload stem assets
    let (midi_outcome, musicxml_outcome) = tokio::try_join!(
        async {
            audio::generate_midi(&state.config, &temp_input_path, temp_dir.path())
                .await
                .map_err(AppError::from)
        },
        async {
            audio::generate_musicxml(&state.config, &temp_input_path, temp_dir.path())
                .await
                .map_err(AppError::from)
        },
    )?;

    let (stem_results, stems_status, stems_error) = audio::generate_stems(
        &state.config,
        &temp_input_path,
        temp_dir.path(),
        quality_profile,
    )
    .await?;

    // Insert the musics row BEFORE running store_stems so the FK constraint is satisfied.
    // Conversion-result columns have DEFAULT values and will be updated below.
    let created_at = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        r#"
        INSERT INTO musics (id, title, filename, content_type, object_key, public_token, public_id, quality_profile, created_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        "#,
    )
    .bind(&music_id)
    .bind(&resolved_title)
    .bind(&filename)
    .bind(&content_type)
    .bind(&object_key)
    .bind(&public_token)
    .bind(&public_id)
    .bind(quality_profile.as_str())
    .bind(&created_at)
    .execute(&state.db_rw)
    .await?;

    let (
        (midi_object_key, midi_status, midi_error),
        (musicxml_object_key, musicxml_status, musicxml_error),
        (stems_status, stems_error),
    ) = tokio::try_join!(
        store_conversion(&state, &music_id, "midi", midi_outcome),
        store_conversion(&state, &music_id, "musicxml", musicxml_outcome),
        store_stems(&state, &music_id, stem_results, stems_status, stems_error),
    )?;

    let audio_object_key = None;
    let audio_status = "disabled".to_owned();
    let audio_error = None;

    // Update conversion results onto the row we just inserted.
    sqlx::query(
        r#"
        UPDATE musics SET
            audio_object_key   = $1, audio_status   = $2, audio_error   = $3,
            midi_object_key    = $4, midi_status    = $5, midi_error    = $6,
            musicxml_object_key = $7, musicxml_status = $8, musicxml_error = $9,
            stems_status       = $10, stems_error    = $11
        WHERE id = $12
        "#,
    )
    .bind(&audio_object_key)
    .bind(&audio_status)
    .bind(&audio_error)
    .bind(&midi_object_key)
    .bind(&midi_status)
    .bind(&midi_error)
    .bind(&musicxml_object_key)
    .bind(&musicxml_status)
    .bind(&musicxml_error)
    .bind(&stems_status)
    .bind(&stems_error)
    .bind(&music_id)
    .execute(&state.db_rw)
    .await?;

    let record = MusicRecord {
        id: music_id,
        title: resolved_title,
        filename,
        content_type,
        object_key,
        audio_object_key,
        audio_status,
        audio_error,
        midi_object_key,
        midi_status,
        midi_error,
        musicxml_object_key,
        musicxml_status,
        musicxml_error,
        stems_status,
        stems_error,
        public_token,
        public_id,
        quality_profile: quality_profile.as_str().to_owned(),
        created_at,
    };

    let stems_total = fetch_stems_total(&state.db_rw, &record.id).await;
    Ok(Json(record_to_admin_response(
        &state.config,
        &state.storage,
        record,
        stems_total,
    )))
}

async fn admin_retry_render(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<AdminMusicResponse>, AppError> {
    require_admin(&headers, &state.config)?;

    let record = find_music_by_id(&state.db_rw, &id)
        .await?
        .ok_or_else(|| AppError::not_found("Music not found"))?;
    let quality_profile =
        audio::StemQualityProfile::from_stored_or_default(&record.quality_profile);

    // Fetch the original score bytes from storage.
    let (score_bytes, _) = state.storage.get_bytes(&record.object_key).await?;

    let safe_filename = sanitize_filename(&record.filename);
    let temp_dir = tempfile::tempdir()?;
    let temp_input_path = temp_dir.path().join(&safe_filename);
    fs::write(&temp_input_path, &score_bytes).await?;

    // Re-run MIDI and MusicXML exports in parallel.
    let (midi_outcome, musicxml_outcome) = tokio::try_join!(
        async {
            audio::generate_midi(&state.config, &temp_input_path, temp_dir.path())
                .await
                .map_err(AppError::from)
        },
        async {
            audio::generate_musicxml(&state.config, &temp_input_path, temp_dir.path())
                .await
                .map_err(AppError::from)
        },
    )?;
    let (midi_object_key, midi_status, midi_error) =
        store_conversion(&state, &id, "midi", midi_outcome).await?;
    let (musicxml_object_key, musicxml_status, musicxml_error) =
        store_conversion(&state, &id, "musicxml", musicxml_outcome).await?;

    // Delete old stems then re-render.
    sqlx::query("DELETE FROM stems WHERE music_id = $1")
        .bind(&id)
        .execute(&state.db_rw)
        .await?;

    let (stem_results, stems_status, stems_error) = audio::generate_stems(
        &state.config,
        &temp_input_path,
        temp_dir.path(),
        quality_profile,
    )
    .await?;

    let (stems_status, stems_error) =
        store_stems(&state, &id, stem_results, stems_status, stems_error).await?;

    sqlx::query(
        "UPDATE musics SET \
         audio_object_key = NULL, audio_status = 'disabled', audio_error = NULL, \
         midi_object_key = $1, midi_status = $2, midi_error = $3, \
         musicxml_object_key = $4, musicxml_status = $5, musicxml_error = $6, \
         stems_status = $7, stems_error = $8 WHERE id = $9",
    )
    .bind(&midi_object_key)
    .bind(&midi_status)
    .bind(&midi_error)
    .bind(&musicxml_object_key)
    .bind(&musicxml_status)
    .bind(&musicxml_error)
    .bind(&stems_status)
    .bind(&stems_error)
    .bind(&id)
    .execute(&state.db_rw)
    .await?;

    let updated = find_music_by_id(&state.db_rw, &id)
        .await?
        .ok_or_else(|| AppError::not_found("Music not found"))?;

    let stems_total = fetch_stems_total(&state.db_rw, &id).await;
    Ok(Json(record_to_admin_response(
        &state.config,
        &state.storage,
        updated,
        stems_total,
    )))
}

async fn store_stems(
    state: &AppState,
    music_id: &str,
    stems: Vec<audio::StemResult>,
    status: String,
    error: Option<String>,
) -> Result<(String, Option<String>), AppError> {
    for stem in stems {
        let size_bytes = stem.bytes.len() as i64;
        let storage_key = stem_full_key(music_id, stem.track_index);
        let drum_map_json = stem
            .drum_map
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| AppError::from(anyhow::Error::from(error)))?;
        state
            .storage
            .upload_bytes(&storage_key, stem.bytes.clone(), "audio/ogg")
            .await?;

        sqlx::query(
            "INSERT INTO stems (music_id, track_index, track_name, instrument_name, storage_key, size_bytes, drum_map_json) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(music_id)
        .bind(stem.track_index as i64)
        .bind(&stem.track_name)
        .bind(&stem.instrument_name)
        .bind(&storage_key)
        .bind(size_bytes)
        .bind(&drum_map_json)
        .execute(&state.db_rw)
        .await?;
    }
    Ok((status, error))
}

async fn probe_audio_duration_seconds(path: &std::path::Path) -> Result<f64, AppError> {
    let output = Command::new("ffprobe")
        .arg("-v")
        .arg("error")
        .arg("-show_entries")
        .arg("format=duration")
        .arg("-of")
        .arg("default=noprint_wrappers=1:nokey=1")
        .arg(path)
        .output()
        .await
        .map_err(AppError::from)?;

    if !output.status.success() {
        return Err(AppError::from(anyhow::anyhow!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    let duration = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .map_err(|error| AppError::from(anyhow::anyhow!("invalid ffprobe duration: {error}")))?;
    Ok(duration)
}

async fn public_music_stems(
    State(state): State<AppState>,
    Path(access_key): Path<String>,
) -> Result<Json<Vec<StemInfo>>, AppError> {
    let record = find_public_music_record(&state, &access_key)
        .await?
        .ok_or_else(|| AppError::not_found("Music not found"))?;

    let stems = find_public_stems(&state.db_ro, &state.db_rw, &record.id).await?;

    let mut resolved_infos = Vec::new();
    for stem in stems {
        let full_stem_url = state
            .storage
            .public_url(&stem.storage_key)
            .unwrap_or_else(|| format!("/api/public/{}/stems/{}", access_key, stem.track_index));
        let duration_seconds =
            if let Some(path) = state.storage.local_path_for_key(&stem.storage_key) {
                probe_audio_duration_seconds(&path).await?
            } else {
                let (stem_bytes, _) = state.storage.get_bytes(&stem.storage_key).await?;
                let temp_dir = tempfile::tempdir()?;
                let full_stem_path = temp_dir.path().join("stem.ogg");
                fs::write(&full_stem_path, stem_bytes).await?;
                probe_audio_duration_seconds(&full_stem_path).await?
            };

        resolved_infos.push(StemInfo {
            track_index: stem.track_index,
            track_name: stem.track_name,
            instrument_name: stem.instrument_name,
            full_stem_url,
            duration_seconds,
            drum_map: stem
                .drum_map_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(|error| AppError::from(anyhow::Error::from(error)))?,
        });
    }

    Ok(Json(resolved_infos))
}

async fn public_music_stem_audio(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((access_key, track_index)): Path<(String, i64)>,
) -> Result<Response, AppError> {
    let record = find_public_music_record(&state, &access_key)
        .await?
        .ok_or_else(|| AppError::not_found("Music not found"))?;

    let stem = find_public_stem(&state.db_ro, &state.db_rw, &record.id, track_index)
        .await?
        .ok_or_else(|| AppError::not_found("Stem not found"))?;

    if let Some(path) = state.storage.local_path_for_key(&stem.storage_key) {
        return local_file_response(
            &path,
            "audio/ogg",
            Some(format!("inline; filename=\"{}.ogg\"", stem.track_name)),
            headers.get(header::RANGE),
        )
        .await;
    }

    let (bytes, content_type) = state.storage.get_bytes(&stem.storage_key).await?;
    Ok(binary_response(
        bytes,
        content_type.unwrap_or_else(|| "audio/ogg".to_owned()),
        Some(format!("inline; filename=\"{}.ogg\"", stem.track_name)),
    ))
}

async fn store_conversion(
    state: &AppState,
    music_id: &str,
    kind: &str,
    outcome: audio::ConversionOutcome,
) -> Result<(Option<String>, String, Option<String>), AppError> {
    match outcome {
        audio::ConversionOutcome::Ready {
            bytes,
            content_type,
            extension,
        } => {
            let object_key = format!("{kind}/{music_id}.{extension}");
            state
                .storage
                .upload_bytes(&object_key, bytes, content_type)
                .await?;
            Ok((Some(object_key), "ready".to_owned(), None))
        }
        audio::ConversionOutcome::Unavailable { reason } => {
            Ok((None, "unavailable".to_owned(), Some(reason)))
        }
        audio::ConversionOutcome::Failed { reason } => {
            warn!("{kind} conversion failed for {music_id}: {reason}");
            Ok((None, "failed".to_owned(), Some(reason)))
        }
    }
}

async fn admin_update_music(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(payload): Json<UpdateMusicRequest>,
) -> Result<Json<AdminMusicResponse>, AppError> {
    require_admin(&headers, &state.config)?;

    let public_id = normalize_public_id(payload.public_id.as_deref())?;
    ensure_public_id_available(&state.db_rw, public_id.as_deref(), Some(&id)).await?;

    let update_result = sqlx::query("UPDATE musics SET public_id = $1 WHERE id = $2")
        .bind(&public_id)
        .bind(&id)
        .execute(&state.db_rw)
        .await?;

    if update_result.rows_affected() == 0 {
        return Err(AppError::not_found("Music not found"));
    }

    let record = find_music_by_id(&state.db_rw, &id)
        .await?
        .ok_or_else(|| AppError::not_found("Music not found"))?;

    let stems_total = fetch_stems_total(&state.db_rw, &id).await;
    Ok(Json(record_to_admin_response(
        &state.config,
        &state.storage,
        record,
        stems_total,
    )))
}

async fn public_music(
    State(state): State<AppState>,
    Path(access_key): Path<String>,
) -> Result<Json<PublicMusicResponse>, AppError> {
    let record = find_public_music_record(&state, &access_key)
        .await?
        .ok_or_else(|| AppError::not_found("Music not found"))?;

    Ok(Json(record_to_public_response(
        &state.storage,
        record,
        &access_key,
    )))
}

async fn public_music_audio(
    State(state): State<AppState>,
    Path(access_key): Path<String>,
) -> Result<Response, AppError> {
    let record = find_public_music_record(&state, &access_key)
        .await?
        .ok_or_else(|| AppError::not_found("Music not found"))?;

    let audio_key = record
        .audio_object_key
        .ok_or_else(|| AppError::not_found("Audio preview is not available for this score"))?;

    let (bytes, content_type) = state.storage.get_bytes(&audio_key).await?;
    Ok(binary_response(
        bytes,
        content_type.unwrap_or_else(|| "audio/mpeg".to_owned()),
        Some("inline; filename=\"preview.mp3\"".to_owned()),
    ))
}

async fn public_music_midi(
    State(state): State<AppState>,
    Path(access_key): Path<String>,
) -> Result<Response, AppError> {
    let record = find_public_music_record(&state, &access_key)
        .await?
        .ok_or_else(|| AppError::not_found("Music not found"))?;

    let midi_key = record
        .midi_object_key
        .ok_or_else(|| AppError::not_found("MIDI export is not available for this score"))?;

    let (bytes, content_type) = state.storage.get_bytes(&midi_key).await?;
    Ok(binary_response(
        bytes,
        content_type.unwrap_or_else(|| "audio/midi".to_owned()),
        Some(format!(
            "attachment; filename=\"{}\"",
            midi_filename_for(&record.filename)
        )),
    ))
}

async fn public_music_musicxml(
    State(state): State<AppState>,
    Path(access_key): Path<String>,
) -> Result<Response, AppError> {
    let record = find_public_music_record(&state, &access_key)
        .await?
        .ok_or_else(|| AppError::not_found("Music not found"))?;

    let musicxml_key = record
        .musicxml_object_key
        .ok_or_else(|| AppError::not_found("MusicXML export is not available for this score"))?;

    let (bytes, content_type) = state.storage.get_bytes(&musicxml_key).await?;
    Ok(binary_response(
        bytes,
        content_type.unwrap_or_else(|| "application/xml".to_owned()),
        // inline so the browser/OSMD can fetch it; filename still set for right-click-save
        Some(format!(
            "inline; filename=\"{}.musicxml\"",
            sanitize_content_disposition(&record.filename.trim_end_matches(".mscz").to_owned())
        )),
    ))
}

async fn public_music_download(
    State(state): State<AppState>,
    Path(access_key): Path<String>,
) -> Result<Response, AppError> {
    let record = find_public_music_record(&state, &access_key)
        .await?
        .ok_or_else(|| AppError::not_found("Music not found"))?;

    let (bytes, content_type) = state.storage.get_bytes(&record.object_key).await?;
    Ok(binary_response(
        bytes,
        content_type.unwrap_or(record.content_type),
        Some(format!(
            "attachment; filename=\"{}\"",
            sanitize_content_disposition(&record.filename)
        )),
    ))
}

fn require_admin(headers: &HeaderMap, config: &AppConfig) -> Result<(), AppError> {
    let Some(header_value) = headers.get("x-admin-password") else {
        return Err(AppError::unauthorized("Missing x-admin-password header"));
    };

    let password = header_value
        .to_str()
        .map_err(|_| AppError::unauthorized("Invalid x-admin-password header"))?;

    if password != config.admin_password {
        return Err(AppError::unauthorized("Invalid admin password"));
    }

    Ok(())
}

async fn ensure_public_id_available(
    db: &PgPool,
    public_id: Option<&str>,
    current_music_id: Option<&str>,
) -> Result<(), AppError> {
    let Some(public_id) = public_id else {
        return Ok(());
    };

    let existing = sqlx::query_scalar::<_, String>("SELECT id FROM musics WHERE public_id = $1")
        .bind(public_id)
        .fetch_optional(db)
        .await?;

    if let Some(existing_id) = existing {
        if Some(existing_id.as_str()) != current_music_id {
            return Err(AppError::conflict("That public id is already in use"));
        }
    }

    Ok(())
}

async fn find_music_by_id(db: &PgPool, id: &str) -> Result<Option<MusicRecord>> {
    Ok(sqlx::query_as::<_, MusicRecord>(
        r#"
        SELECT id, title, filename, content_type, object_key, audio_object_key, audio_status, audio_error, midi_object_key, midi_status, midi_error, musicxml_object_key, musicxml_status, musicxml_error, stems_status, stems_error, public_token, public_id, quality_profile, created_at
        FROM musics
        WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(db)
    .await?)
}

async fn find_music_by_access_key(db: &PgPool, access_key: &str) -> Result<Option<MusicRecord>> {
    Ok(sqlx::query_as::<_, MusicRecord>(
        r#"
        SELECT id, title, filename, content_type, object_key, audio_object_key, audio_status, audio_error, midi_object_key, midi_status, midi_error, musicxml_object_key, musicxml_status, musicxml_error, stems_status, stems_error, public_token, public_id, quality_profile, created_at
        FROM musics
        WHERE public_token = $1 OR public_id = $2
        LIMIT 1
        "#,
    )
    .bind(access_key)
    .bind(access_key)
    .fetch_optional(db)
    .await?)
}

fn record_to_admin_response(
    config: &AppConfig,
    storage: &Storage,
    record: MusicRecord,
    stems_total_bytes: i64,
) -> AdminMusicResponse {
    let public_id_url = record
        .public_id
        .as_ref()
        .map(|public_id| config.public_url_for(public_id));
    let midi_download_url = record.midi_object_key.as_ref().map(|object_key| {
        storage
            .public_url(object_key)
            .unwrap_or_else(|| format!("/api/public/{}/midi", record.public_token))
    });
    let download_url = storage
        .public_url(&record.object_key)
        .unwrap_or_else(|| format!("/api/public/{}/download", record.public_token));

    AdminMusicResponse {
        id: record.id,
        title: record.title,
        filename: record.filename,
        content_type: record.content_type,
        audio_status: record.audio_status,
        audio_error: record.audio_error,
        midi_status: record.midi_status,
        midi_error: record.midi_error,
        musicxml_status: record.musicxml_status,
        musicxml_error: record.musicxml_error,
        stems_status: record.stems_status,
        stems_error: record.stems_error,
        public_token: record.public_token.clone(),
        public_id: record.public_id,
        public_url: config.public_url_for(&record.public_token),
        public_id_url,
        download_url,
        midi_download_url,
        quality_profile: record.quality_profile,
        created_at: record.created_at,
        stems_total_bytes,
    }
}

fn record_to_public_response(
    storage: &Storage,
    record: MusicRecord,
    access_key: &str,
) -> PublicMusicResponse {
    let midi_download_url = record.midi_object_key.as_ref().map(|object_key| {
        storage
            .public_url(object_key)
            .unwrap_or_else(|| format!("/api/public/{access_key}/midi"))
    });
    let musicxml_url = record.musicxml_object_key.as_ref().map(|object_key| {
        storage
            .public_url(object_key)
            .unwrap_or_else(|| format!("/api/public/{access_key}/musicxml"))
    });
    let download_url = storage
        .public_url(&record.object_key)
        .unwrap_or_else(|| format!("/api/public/{access_key}/download"));

    PublicMusicResponse {
        title: record.title,
        filename: record.filename,
        audio_status: "disabled".to_owned(),
        audio_error: None,
        can_stream_audio: false,
        audio_stream_url: None,
        midi_status: record.midi_status,
        midi_error: record.midi_error,
        midi_download_url,
        musicxml_url,
        stems_status: record.stems_status,
        stems_error: record.stems_error,
        download_url,
        created_at: record.created_at,
    }
}

fn generate_public_token() -> String {
    rand::rng()
        .sample_iter(Alphanumeric)
        .take(24)
        .map(char::from)
        .collect()
}

fn parse_quality_profile(raw: Option<&str>) -> Result<audio::StemQualityProfile, AppError> {
    let value = raw
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(audio::DEFAULT_STEM_QUALITY_PROFILE);

    audio::StemQualityProfile::from_slug(value).ok_or_else(|| {
        AppError::bad_request("Invalid quality profile. Use one of: compact, standard, high.")
    })
}

fn normalize_public_id(raw: Option<&str>) -> Result<Option<String>, AppError> {
    let Some(value) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };

    if !(3..=64).contains(&value.len()) {
        return Err(AppError::bad_request(
            "Public ids must be between 3 and 64 characters",
        ));
    }

    if !value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '-' || character == '_')
    {
        return Err(AppError::bad_request(
            "Public ids can only contain letters, numbers, hyphens, and underscores",
        ));
    }

    Ok(Some(value.to_ascii_lowercase()))
}

fn sanitize_filename(filename: &str) -> String {
    let mut sanitized = filename
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric()
                || character == '.'
                || character == '-'
                || character == '_'
            {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();

    if sanitized.is_empty() {
        sanitized = "score.mscz".to_owned();
    }

    sanitized
}

fn sanitize_content_disposition(filename: &str) -> String {
    filename.replace('"', "")
}

fn midi_filename_for(filename: &str) -> String {
    let stem = filename
        .trim_end_matches(".mscz")
        .trim_end_matches(".MSCZ")
        .trim_end_matches(".mscx")
        .trim_end_matches(".MSCX");
    sanitize_content_disposition(&format!("{stem}.mid"))
}

fn stem_full_key(music_id: &str, track_index: usize) -> String {
    format!("stems/{music_id}/{track_index}.ogg")
}

fn binary_response(
    bytes: Bytes,
    content_type: String,
    content_disposition: Option<String>,
) -> Response {
    let mut response = Response::new(axum::body::Body::from(bytes));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );

    if let Some(content_disposition) = content_disposition {
        if let Ok(value) = HeaderValue::from_str(&content_disposition) {
            response
                .headers_mut()
                .insert(header::CONTENT_DISPOSITION, value);
        }
    }

    response
}

async fn local_file_response(
    path: &std::path::Path,
    content_type: &str,
    content_disposition: Option<String>,
    range_header: Option<&HeaderValue>,
) -> Result<Response, AppError> {
    let metadata = tokio::fs::metadata(path).await.map_err(AppError::from)?;
    let file_len = metadata.len();

    let parsed_range = range_header
        .map(|value| parse_byte_range_header(value, file_len))
        .transpose()?
        .flatten();

    let (start, end, status) = match parsed_range {
        Some((start, end)) => (start, end, StatusCode::PARTIAL_CONTENT),
        None if file_len == 0 => (0, 0, StatusCode::OK),
        None => (0, file_len - 1, StatusCode::OK),
    };

    let byte_count = if file_len == 0 {
        0usize
    } else {
        (end - start + 1) as usize
    };

    let mut file = tokio::fs::File::open(path).await.map_err(AppError::from)?;
    if byte_count > 0 {
        file.seek(std::io::SeekFrom::Start(start))
            .await
            .map_err(AppError::from)?;
    }

    let mut bytes = vec![0u8; byte_count];
    if byte_count > 0 {
        file.read_exact(&mut bytes).await.map_err(AppError::from)?;
    }

    let mut response = binary_response(
        Bytes::from(bytes),
        content_type.to_owned(),
        content_disposition,
    );
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&byte_count.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );

    if status == StatusCode::PARTIAL_CONTENT {
        let content_range = format!("bytes {start}-{end}/{file_len}");
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&content_range)
                .unwrap_or_else(|_| HeaderValue::from_static("bytes */0")),
        );
    }

    Ok(response)
}

fn parse_byte_range_header(
    value: &HeaderValue,
    file_len: u64,
) -> Result<Option<(u64, u64)>, AppError> {
    if file_len == 0 {
        return Ok(None);
    }

    let value = value
        .to_str()
        .map_err(|_| AppError::bad_request("Invalid Range header"))?
        .trim();

    let range_spec = value
        .strip_prefix("bytes=")
        .ok_or_else(|| AppError::bad_request("Only bytes ranges are supported"))?;

    if range_spec.contains(',') {
        return Err(AppError::bad_request(
            "Multiple byte ranges are not supported",
        ));
    }

    let (start_raw, end_raw) = range_spec
        .split_once('-')
        .ok_or_else(|| AppError::bad_request("Invalid Range header"))?;

    let invalid_range = || {
        AppError::new(
            StatusCode::RANGE_NOT_SATISFIABLE,
            format!("Requested range is not satisfiable for a {file_len}-byte file"),
        )
    };

    let range = if start_raw.is_empty() {
        let suffix_len = end_raw
            .parse::<u64>()
            .map_err(|_| AppError::bad_request("Invalid Range header"))?;
        if suffix_len == 0 {
            return Err(invalid_range());
        }
        let start = file_len.saturating_sub(suffix_len);
        (start, file_len - 1)
    } else {
        let start = start_raw
            .parse::<u64>()
            .map_err(|_| AppError::bad_request("Invalid Range header"))?;
        if start >= file_len {
            return Err(invalid_range());
        }

        let end = if end_raw.is_empty() {
            file_len - 1
        } else {
            let parsed_end = end_raw
                .parse::<u64>()
                .map_err(|_| AppError::bad_request("Invalid Range header"))?;
            if parsed_end < start {
                return Err(invalid_range());
            }
            parsed_end.min(file_len - 1)
        };

        (start, end)
    };

    Ok(Some(range))
}
