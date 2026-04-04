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
    AdminMusicResponse, LoginRequest, LoginResponse, MusicRecord, PublicMusicResponse,
    StemInfo, StemRecord, UpdateMusicRequest,
};
use rand::{Rng, distr::Alphanumeric};
use sqlx::{PgPool, postgres::{PgConnectOptions, PgPoolOptions}};
use std::str::FromStr;
use std::{net::SocketAddr, path::PathBuf};
use storage::Storage;
use tokio::fs;
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
            std::env::var("RUST_LOG").unwrap_or_else(|_| "backend=info,tower_http=info".to_owned()),
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
        .route("/public/{access_key}/stems/{track_index}", get(public_music_stem_audio))
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
            size_bytes BIGINT NOT NULL DEFAULT 0
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
    ensure_stems_column(db, "size_bytes", "BIGINT NOT NULL DEFAULT 0").await?;

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

async fn find_public_music_record(state: &AppState, access_key: &str) -> Result<Option<MusicRecord>, AppError> {
    if let Some(record) = find_music_by_access_key(&state.db_ro, access_key).await? {
        return Ok(Some(record));
    }

    Ok(find_music_by_access_key(&state.db_rw, access_key).await?)
}

async fn find_public_stems(db_primary: &PgPool, db_fallback: &PgPool, music_id: &str) -> Result<Vec<StemRecord>, AppError> {
    let query = "SELECT id, music_id, track_index, track_name, instrument_name, storage_key \
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
    let query = "SELECT id, music_id, track_index, track_name, instrument_name, storage_key \
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
        SELECT id, title, filename, content_type, object_key, audio_object_key, audio_status, audio_error, midi_object_key, midi_status, midi_error, musicxml_object_key, musicxml_status, musicxml_error, stems_status, stems_error, public_token, public_id, created_at
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
    let totals: std::collections::HashMap<String, i64> =
        total_rows.into_iter().map(|r| (r.music_id, r.total_bytes)).collect();

    let items = rows
        .into_iter()
        .map(|record| {
            let total = totals.get(&record.id).copied().unwrap_or(0);
            record_to_admin_response(&state.config, record, total)
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
    let mut upload: Option<(String, String, Bytes)> = None;

    while let Some(field) = multipart.next_field().await? {
        match field.name() {
            Some("title") => {
                title = Some(field.text().await?.trim().to_owned());
            }
            Some("public_id") => {
                requested_public_id = Some(field.text().await?.trim().to_owned());
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
    //   t=T_midi+T_stems → four-way parallel:
    //                      • upload MIDI
    //                      • upload MusicXML
    //                      • mix stem WAVs → MP3 preview via ffmpeg
    //                      • upload stem OGGs
    let (midi_outcome, musicxml_outcome) = tokio::try_join!(
        async { audio::generate_midi(&state.config, &temp_input_path, temp_dir.path()).await.map_err(AppError::from) },
        async { audio::generate_musicxml(&state.config, &temp_input_path, temp_dir.path()).await.map_err(AppError::from) },
    )?;

    let (stem_results, stems_status, stems_error) =
        audio::generate_stems(&state.config, &temp_input_path, temp_dir.path()).await?;

    // Insert the musics row BEFORE running store_stems so the FK constraint is satisfied.
    // Conversion-result columns have DEFAULT values and will be updated below.
    let created_at = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        r#"
        INSERT INTO musics (id, title, filename, content_type, object_key, public_token, public_id, created_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        "#,
    )
    .bind(&music_id)
    .bind(&resolved_title)
    .bind(&filename)
    .bind(&content_type)
    .bind(&object_key)
    .bind(&public_token)
    .bind(&public_id)
    .bind(&created_at)
    .execute(&state.db_rw)
    .await?;

    let (
        (midi_object_key, midi_status, midi_error),
        (musicxml_object_key, musicxml_status, musicxml_error),
        (audio_object_key, audio_status, audio_error),
        (stems_status, stems_error),
    ) = tokio::try_join!(
        store_conversion(&state, &music_id, "midi", midi_outcome),
        store_conversion(&state, &music_id, "musicxml", musicxml_outcome),
        async {
            let outcome = audio::mix_stems_to_preview(temp_dir.path())
                .await
                .map_err(AppError::from)?;
            store_conversion(&state, &music_id, "audio", outcome).await
        },
        store_stems(&state, &music_id, stem_results, stems_status, stems_error),
    )?;

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
        created_at,
    };

    let stems_total = fetch_stems_total(&state.db_rw, &record.id).await;
    Ok(Json(record_to_admin_response(&state.config, record, stems_total)))
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

    // Fetch the original score bytes from storage.
    let (score_bytes, _) = state.storage.get_bytes(&record.object_key).await?;

    let safe_filename = sanitize_filename(&record.filename);
    let temp_dir = tempfile::tempdir()?;
    let temp_input_path = temp_dir.path().join(&safe_filename);
    fs::write(&temp_input_path, &score_bytes).await?;

    // Re-run MIDI and MusicXML exports in parallel.
    let (midi_outcome, musicxml_outcome) = tokio::try_join!(
        async { audio::generate_midi(&state.config, &temp_input_path, temp_dir.path()).await.map_err(AppError::from) },
        async { audio::generate_musicxml(&state.config, &temp_input_path, temp_dir.path()).await.map_err(AppError::from) },
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

    let (stem_results, stems_status, stems_error) =
        audio::generate_stems(&state.config, &temp_input_path, temp_dir.path()).await?;

    let (stems_status, stems_error) =
        store_stems(&state, &id, stem_results, stems_status, stems_error).await?;

    sqlx::query(
        "UPDATE musics SET \
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
    Ok(Json(record_to_admin_response(&state.config, updated, stems_total)))
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
        let storage_key = format!("stems/{}/{}.ogg", music_id, stem.track_index);
        state
            .storage
            .upload_bytes(&storage_key, stem.bytes, "audio/ogg")
            .await?;
        sqlx::query(
            "INSERT INTO stems (music_id, track_index, track_name, instrument_name, storage_key, size_bytes) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(music_id)
        .bind(stem.track_index as i64)
        .bind(&stem.track_name)
        .bind(&stem.instrument_name)
        .bind(&storage_key)
        .bind(size_bytes)
        .execute(&state.db_rw)
        .await?;
    }
    Ok((status, error))
}

async fn public_music_stems(
    State(state): State<AppState>,
    Path(access_key): Path<String>,
) -> Result<Json<Vec<StemInfo>>, AppError> {
    let record = find_public_music_record(&state, &access_key)
        .await?
        .ok_or_else(|| AppError::not_found("Music not found"))?;

    let stems = find_public_stems(&state.db_ro, &state.db_rw, &record.id).await?;

    let infos = stems
        .into_iter()
        .map(|s| StemInfo {
            track_index: s.track_index,
            track_name: s.track_name,
            instrument_name: s.instrument_name,
            stream_url: format!("/api/public/{}/stems/{}", access_key, s.track_index),
        })
        .collect();

    Ok(Json(infos))
}

async fn public_music_stem_audio(
    State(state): State<AppState>,
    Path((access_key, track_index)): Path<(String, i64)>,
) -> Result<Response, AppError> {
    let record = find_public_music_record(&state, &access_key)
        .await?
        .ok_or_else(|| AppError::not_found("Music not found"))?;

    let stem = find_public_stem(&state.db_ro, &state.db_rw, &record.id, track_index)
        .await?
        .ok_or_else(|| AppError::not_found("Stem not found"))?;

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
    Ok(Json(record_to_admin_response(&state.config, record, stems_total)))
}

async fn public_music(
    State(state): State<AppState>,
    Path(access_key): Path<String>,
) -> Result<Json<PublicMusicResponse>, AppError> {
    let record = find_public_music_record(&state, &access_key)
        .await?
        .ok_or_else(|| AppError::not_found("Music not found"))?;

    Ok(Json(record_to_public_response(record, &access_key)))
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
        SELECT id, title, filename, content_type, object_key, audio_object_key, audio_status, audio_error, midi_object_key, midi_status, midi_error, musicxml_object_key, musicxml_status, musicxml_error, stems_status, stems_error, public_token, public_id, created_at
        FROM musics
        WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(db)
    .await?)
}

async fn find_music_by_access_key(
    db: &PgPool,
    access_key: &str,
) -> Result<Option<MusicRecord>> {
    Ok(sqlx::query_as::<_, MusicRecord>(
        r#"
        SELECT id, title, filename, content_type, object_key, audio_object_key, audio_status, audio_error, midi_object_key, midi_status, midi_error, musicxml_object_key, musicxml_status, musicxml_error, stems_status, stems_error, public_token, public_id, created_at
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

fn record_to_admin_response(config: &AppConfig, record: MusicRecord, stems_total_bytes: i64) -> AdminMusicResponse {
    let public_id_url = record
        .public_id
        .as_ref()
        .map(|public_id| config.public_url_for(public_id));
    let midi_download_url = record
        .midi_object_key
        .as_ref()
        .map(|_| format!("/api/public/{}/midi", record.public_token));

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
        download_url: format!("/api/public/{}/download", record.public_token),
        midi_download_url,
        created_at: record.created_at,
        stems_total_bytes,
    }
}

fn record_to_public_response(record: MusicRecord, access_key: &str) -> PublicMusicResponse {
    PublicMusicResponse {
        title: record.title,
        filename: record.filename,
        audio_status: record.audio_status,
        audio_error: record.audio_error,
        can_stream_audio: record.audio_object_key.is_some(),
        audio_stream_url: record
            .audio_object_key
            .map(|_| format!("/api/public/{access_key}/audio")),
        midi_status: record.midi_status,
        midi_error: record.midi_error,
        midi_download_url: record
            .midi_object_key
            .map(|_| format!("/api/public/{access_key}/midi")),
        musicxml_url: record
            .musicxml_object_key
            .map(|_| format!("/api/public/{access_key}/musicxml")),
        stems_status: record.stems_status,
        stems_error: record.stems_error,
        download_url: format!("/api/public/{access_key}/download"),
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
