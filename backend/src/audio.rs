use crate::config::AppConfig;
use anyhow::{Context, Result};
use bytes::Bytes;
use midly::{MetaMessage, MidiMessage, Smf, TrackEventKind};
use std::path::{Path, PathBuf};
use tokio::process::Command;

pub enum ConversionOutcome {
    Ready {
        bytes: Bytes,
        content_type: &'static str,
        extension: &'static str,
    },
    Unavailable {
        reason: String,
    },
    Failed {
        reason: String,
    },
}

pub struct StemResult {
    pub track_index: usize,
    pub track_name: String,
    pub instrument_name: String,
    pub bytes: Bytes,
}

struct TrackInfo {
    /// Index into the raw MIDI track chunks list (chunk 0 is the tempo track).
    midi_track_index: usize,
    track_name: String,
    program: u8,
    is_percussion: bool,
}

// ---------------------------------------------------------------------------
// Public async entry points
// ---------------------------------------------------------------------------

pub async fn generate_midi(
    config: &AppConfig,
    input_path: &Path,
    output_dir: &Path,
) -> Result<ConversionOutcome> {
    convert_with_musescore(
        config,
        input_path,
        &output_dir.join("preview.mid"),
        "audio/midi",
        "mid",
        "MuseScore CLI not configured. Set MUSESCORE_BIN to enable MIDI conversion.",
    )
    .await
}

pub async fn generate_musicxml(
    config: &AppConfig,
    input_path: &Path,
    output_dir: &Path,
) -> Result<ConversionOutcome> {
    convert_with_musescore(
        config,
        input_path,
        &output_dir.join("score.musicxml"),
        "application/xml",
        "musicxml",
        "MuseScore CLI not configured. Set MUSESCORE_BIN to enable MusicXML export.",
    )
    .await
}

/// Render per-instrument OGG stems using sfizz + VSCO-2-CE SFZ soundfonts.
///
/// Reuses `output_dir/preview.mid` if it already exists from a prior
/// `generate_midi` call, avoiding a redundant MuseScore invocation.
///
/// Returns `(stems, status, error_message)`.
/// `status` is one of `"unavailable"`, `"ready"`, or `"failed"`.
pub async fn generate_stems(
    config: &AppConfig,
    input_path: &Path,
    output_dir: &Path,
) -> Result<(Vec<StemResult>, String, Option<String>)> {
    // --- pre-flight checks ---------------------------------------------------

    tracing::info!("stems: starting pipeline for '{}'", input_path.file_name().unwrap_or_default().to_string_lossy());

    let sfizz = match find_sfizz_binary(config).await {
        Some(bin) => bin,
        None => {
            let reason = "sfizz_render not found. Install sfizz and add sfizz_render to PATH \
                (or set SFIZZ_BIN)."
                .to_owned();
            tracing::warn!("{reason}");
            return Ok((Vec::new(), "unavailable".to_owned(), Some(reason)));
        }
    };

    let sfz_dir = match find_soundfont_dir(config) {
        Some(dir) => dir,
        None => {
            let reason = "VSCO-2-CE soundfont directory not found. \
                Set SOUNDFONT_DIR to point at the VSCO-2-CE-1.1.0 directory."
                .to_owned();
            tracing::warn!("{reason}");
            return Ok((Vec::new(), "unavailable".to_owned(), Some(reason)));
        }
    };

    let ffmpeg = match find_ffmpeg_binary().await {
        Some(bin) => bin,
        None => {
            let reason = "ffmpeg not found. Install ffmpeg and ensure it is in PATH.".to_owned();
            tracing::warn!("{reason}");
            return Ok((Vec::new(), "unavailable".to_owned(), Some(reason)));
        }
    };

    // --- obtain MIDI data ----------------------------------------------------

    let midi_path = output_dir.join("preview.mid");
    tracing::info!("stems: exporting MIDI from score");
    let midi_bytes: Bytes = if midi_path.exists() {
        tracing::info!("stems: reusing existing MIDI file");
        tokio::fs::read(&midi_path)
            .await
            .context("reading existing MIDI file")?
            .into()
    } else {
        match generate_midi(config, input_path, output_dir).await? {
            ConversionOutcome::Ready { bytes, .. } => {
                tracing::info!("stems: MIDI export complete");
                bytes
            }
            ConversionOutcome::Unavailable { reason } => {
                return Ok((Vec::new(), "unavailable".to_owned(), Some(reason)));
            }
            ConversionOutcome::Failed { reason } => {
                return Ok((Vec::new(), "failed".to_owned(), Some(reason)));
            }
        }
    };

    // --- parse MIDI structure ------------------------------------------------

    let track_infos = parse_midi_tracks(&midi_bytes);
    if track_infos.is_empty() {
        return Ok((
            Vec::new(),
            "unavailable".to_owned(),
            Some("No instrument tracks found in MIDI export".to_owned()),
        ));
    }

    let chunks = extract_raw_midi_chunks(&midi_bytes);
    if chunks.len() <= 1 {
        return Ok((
            Vec::new(),
            "unavailable".to_owned(),
            Some(
                "MIDI export is single-track (Format 0); \
                per-instrument stems require a multi-track Format 1 export"
                    .to_owned(),
            ),
        ));
    }

    // Build a clean tempo track (meta events only) from chunk 0 so that note
    // events in MuseScore's combined conductor+instrument track don't bleed
    // into every other stem.
    let clean_tempo_chunk = strip_channel_events(chunks[0]);

    let total = track_infos.len();
    tracing::info!("stems: found {total} instrument tracks, starting render");

    // --- build per-stem tasks (plan phase, synchronous) ---------------------

    struct StemTask {
        stem_idx: usize,
        #[allow(dead_code)]
        chunk_idx: usize,
        track_name: String,
        program: u8,
        is_percussion: bool,
        sfz_path: PathBuf,
        stem_midi: Vec<u8>,
        stem_mid_path: PathBuf,
        stem_wav_path: PathBuf,
        stem_ogg_path: PathBuf,
        sfizz: String,
        ffmpeg: String,
    }

    let mut task_list: Vec<StemTask> = Vec::new();

    for (stem_idx, track_info) in track_infos.iter().enumerate() {
        let chunk_idx = track_info.midi_track_index;
        if chunk_idx >= chunks.len() {
            continue;
        }

        let sfz_path =
            match sfz_for_gm_program(track_info.program, track_info.is_percussion, &sfz_dir) {
                Some(path) => path,
                None => {
                    tracing::info!(
                        "stems: [{}/{}] '{}' ({}, GM prog {}) – no SFZ mapping, skipping",
                        stem_idx + 1,
                        total,
                        track_info.track_name,
                        gm_instrument_name(track_info.program, track_info.is_percussion),
                        track_info.program,
                    );
                    continue;
                }
            };

        tracing::info!(
            "stems: [{}/{}] '{}' ({}, GM prog {}) – queued for render with {}",
            stem_idx + 1,
            total,
            track_info.track_name,
            gm_instrument_name(track_info.program, track_info.is_percussion),
            track_info.program,
            sfz_path.file_name().unwrap_or_default().to_string_lossy(),
        );

        // 2-track Format-1 MIDI: clean tempo track + this instrument track
        let stem_midi = build_stem_midi(&midi_bytes, &clean_tempo_chunk, chunks[chunk_idx]);

        task_list.push(StemTask {
            stem_idx,
            chunk_idx,
            track_name: track_info.track_name.clone(),
            program: track_info.program,
            is_percussion: track_info.is_percussion,
            sfz_path,
            stem_midi,
            stem_mid_path: output_dir.join(format!("stem_{chunk_idx}.mid")),
            stem_wav_path: output_dir.join(format!("stem_{chunk_idx}.wav")),
            stem_ogg_path: output_dir.join(format!("stem_{chunk_idx}.ogg")),
            sfizz: sfizz.clone(),
            ffmpeg: ffmpeg.clone(),
        });
    }

    // --- spawn all stems in parallel ----------------------------------------

    let mut handles = Vec::with_capacity(task_list.len());

    for task in task_list {
        handles.push(tokio::spawn(async move {
            // Write stem MIDI
            if let Err(e) = tokio::fs::write(&task.stem_mid_path, &task.stem_midi).await {
                tracing::warn!(
                    "stems: [{}/{}] '{}' – writing MIDI failed: {e}",
                    task.stem_idx + 1,
                    total,
                    task.track_name,
                );
                return None;
            }

            // Render WAV with sfizz_render
            match Command::new(&task.sfizz)
                .arg("--sfz")
                .arg(&task.sfz_path)
                .arg("--midi")
                .arg(&task.stem_mid_path)
                .arg("--wav")
                .arg(&task.stem_wav_path)
                .arg("--samplerate")
                .arg("44100")
                .output()
                .await
            {
                Ok(out) if out.status.success() => {}
                Ok(out) => {
                    tracing::warn!(
                        "stems: [{}/{}] '{}' – sfizz_render failed: {}",
                        task.stem_idx + 1,
                        total,
                        task.track_name,
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                    return None;
                }
                Err(e) => {
                    tracing::warn!(
                        "stems: [{}/{}] '{}' – sfizz_render spawn error: {e}",
                        task.stem_idx + 1,
                        total,
                        task.track_name,
                    );
                    return None;
                }
            }

            // Encode WAV → Opus
            match Command::new(&task.ffmpeg)
                .arg("-y")
                .arg("-i")
                .arg(&task.stem_wav_path)
                .arg("-c:a")
                .arg("libopus")
                .arg("-b:a")
                .arg("64k")
                .arg("-ac")
                .arg("1")
                .arg("-application")
                .arg("audio")
                .arg("-ar")
                .arg("48000")
                .arg(&task.stem_ogg_path)
                .output()
                .await
            {
                Ok(out) if out.status.success() => {}
                Ok(out) => {
                    tracing::warn!(
                        "stems: [{}/{}] '{}' – ffmpeg failed: {}",
                        task.stem_idx + 1,
                        total,
                        task.track_name,
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                    return None;
                }
                Err(e) => {
                    tracing::warn!(
                        "stems: [{}/{}] '{}' – ffmpeg spawn error: {e}",
                        task.stem_idx + 1,
                        total,
                        task.track_name,
                    );
                    return None;
                }
            }

            match tokio::fs::read(&task.stem_ogg_path).await {
                Ok(ogg_bytes) => {
                    tracing::info!(
                        "stems: [{}/{}] '{}' – done ({} KB)",
                        task.stem_idx + 1,
                        total,
                        task.track_name,
                        ogg_bytes.len() / 1024,
                    );
                    Some(StemResult {
                        track_index: task.stem_idx,
                        track_name: task.track_name,
                        instrument_name: gm_instrument_name(task.program, task.is_percussion)
                            .to_owned(),
                        bytes: Bytes::from(ogg_bytes),
                    })
                }
                Err(e) => {
                    tracing::warn!(
                        "stems: [{}/{}] '{}' – reading OGG failed: {e}",
                        task.stem_idx + 1,
                        total,
                        task.track_name,
                    );
                    None
                }
            }
        }));
    }

    // --- collect results, preserve original track order ---------------------

    let mut stems: Vec<StemResult> = Vec::new();
    for handle in handles {
        if let Ok(Some(result)) = handle.await {
            stems.push(result);
        }
    }
    stems.sort_by_key(|s| s.track_index);

    if stems.is_empty() {
        tracing::warn!("stems: render complete – 0/{total} stems produced");
        Ok((
            Vec::new(),
            "failed".to_owned(),
            Some(
                "No stems could be rendered – verify sfizz, ffmpeg, and soundfont paths."
                    .to_owned(),
            ),
        ))
    } else {
        tracing::info!("stems: render complete – {}/{total} stems produced", stems.len());
        Ok((stems, "ready".to_owned(), None))
    }
}

/// Mix all `stem_*.wav` files in `output_dir` into a single MP3 preview using
/// ffmpeg. Called after `generate_stems` so the WAVs are already on disk.
/// Returns `ConversionOutcome::Unavailable` when no WAV files exist (e.g.
/// sfizz is not configured).
pub async fn mix_stems_to_preview(output_dir: &Path) -> Result<ConversionOutcome> {
    let ffmpeg = match find_ffmpeg_binary().await {
        Some(bin) => bin,
        None => {
            return Ok(ConversionOutcome::Unavailable {
                reason: "ffmpeg not found; cannot mix stems into preview.".to_owned(),
            });
        }
    };

    // Collect all stem WAV files produced by generate_stems.
    let mut wav_paths: Vec<PathBuf> = Vec::new();
    let mut read_dir = tokio::fs::read_dir(output_dir)
        .await
        .context("reading output dir for stem WAVs")?;
    while let Some(entry) = read_dir.next_entry().await? {
        let path = entry.path();
        if path.extension().map_or(false, |e| e == "wav")
            && path
                .file_name()
                .map_or(false, |n| n.to_string_lossy().starts_with("stem_"))
        {
            wav_paths.push(path);
        }
    }

    if wav_paths.is_empty() {
        return Ok(ConversionOutcome::Unavailable {
            reason: "No stem WAV files found; stems may not have rendered.".to_owned(),
        });
    }

    wav_paths.sort();
    let n = wav_paths.len();
    let preview_path = output_dir.join("preview.mp3");

    let mut cmd = Command::new(&ffmpeg);
    cmd.arg("-y");
    for path in &wav_paths {
        cmd.arg("-i").arg(path);
    }
    if n > 1 {
        cmd.arg("-filter_complex")
            .arg(format!("amix=inputs={n}:duration=longest:normalize=0"));
    }
    cmd.arg("-c:a")
        .arg("libmp3lame")
        .arg("-b:a")
        .arg("192k")
        .arg(&preview_path);

    let out = cmd.output().await.context("ffmpeg mix spawn error")?;
    if !out.status.success() {
        return Ok(ConversionOutcome::Failed {
            reason: format!(
                "ffmpeg stem mix failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        });
    }

    let bytes = tokio::fs::read(&preview_path)
        .await
        .context("reading preview MP3")?;

    tracing::info!("stems: mixed {n} WAV(s) into preview MP3 ({} KB)", bytes.len() / 1024);
    Ok(ConversionOutcome::Ready {
        bytes: Bytes::from(bytes),
        content_type: "audio/mpeg",
        extension: "mp3",
    })
}

// ---------------------------------------------------------------------------
// MIDI helpers
// ---------------------------------------------------------------------------

/// Parse per-track metadata (name, GM program, percussion flag) from MIDI bytes.
/// Includes ALL tracks — MuseScore sometimes puts the first instrument's notes
/// in the conductor track (track 0) instead of a clean tempo-only track.
fn parse_midi_tracks(midi_bytes: &[u8]) -> Vec<TrackInfo> {
    let smf = match Smf::parse(midi_bytes) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("MIDI parse error: {e}");
            return Vec::new();
        }
    };

    let mut result = Vec::new();

    for (i, track) in smf.tracks.iter().enumerate() {
        let mut track_name = format!("Track {i}");
        let mut program: u8 = 0;
        let mut program_set = false;
        let mut is_percussion = false;
        let mut has_notes = false;

        for event in track {
            match &event.kind {
                TrackEventKind::Meta(MetaMessage::TrackName(bytes)) => {
                    let name = String::from_utf8_lossy(bytes).trim().to_owned();
                    if !name.is_empty() {
                        track_name = name;
                    }
                }
                TrackEventKind::Midi {
                    channel,
                    message: MidiMessage::ProgramChange { program: prog },
                } => {
                    let ch = u8::from(*channel);
                    if ch == 9 {
                        is_percussion = true;
                    } else if !program_set {
                        // Use the FIRST program change as the canonical instrument
                        program = u8::from(*prog);
                        program_set = true;
                    }
                }
                TrackEventKind::Midi {
                    channel,
                    message: MidiMessage::NoteOn { vel, .. },
                } => {
                    if u8::from(*vel) > 0 {
                        has_notes = true;
                    }
                    if u8::from(*channel) == 9 {
                        is_percussion = true;
                    }
                }
                TrackEventKind::Midi { channel, .. } => {
                    if u8::from(*channel) == 9 {
                        is_percussion = true;
                    }
                }
                _ => {}
            }
        }

        // Only include tracks that actually have notes to render
        if !has_notes {
            continue;
        }

        result.push(TrackInfo {
            midi_track_index: i,
            track_name,
            program,
            is_percussion,
        });
    }

    result
}

/// Return a slice into `midi_bytes` for each MTrk chunk (including its 8-byte header).
fn extract_raw_midi_chunks(midi_bytes: &[u8]) -> Vec<&[u8]> {
    let mut chunks = Vec::new();
    let mut pos = 0;
    while pos + 8 <= midi_bytes.len() {
        let tag = &midi_bytes[pos..pos + 4];
        let length = u32::from_be_bytes([
            midi_bytes[pos + 4],
            midi_bytes[pos + 5],
            midi_bytes[pos + 6],
            midi_bytes[pos + 7],
        ]) as usize;
        let end = pos + 8 + length;
        if end > midi_bytes.len() {
            break;
        }
        if tag == b"MTrk" {
            chunks.push(&midi_bytes[pos..end]);
        }
        pos = end;
    }
    chunks
}

/// Return a new MTrk chunk that contains only meta events from `chunk`, stripping
/// all MIDI channel messages (Note On/Off, Program Change, Control Change, etc.).
/// This prevents MuseScore's first-instrument notes from bleeding into every stem
/// when the conductor/tempo track also carries instrument data.
fn strip_channel_events(chunk: &[u8]) -> Vec<u8> {
    if chunk.len() < 8 || &chunk[0..4] != b"MTrk" {
        return chunk.to_vec();
    }

    let mut out_events: Vec<u8> = Vec::new();
    let mut p = 8usize; // skip 8-byte MTrk header
    let end = chunk.len();
    let mut running_status: u8 = 0;

    while p < end {
        // Read VLQ delta time
        let delta_start = p;
        loop {
            if p >= end {
                break;
            }
            let b = chunk[p];
            p += 1;
            if b & 0x80 == 0 {
                break;
            }
        }
        let delta_bytes = &chunk[delta_start..p];

        if p >= end {
            break;
        }

        let b0 = chunk[p];

        if b0 == 0xFF {
            // Meta event — KEEP
            let ev_start = p;
            p += 2; // 0xFF + type byte
            // VLQ length
            let mut meta_len: usize = 0;
            loop {
                if p >= end {
                    break;
                }
                let b = chunk[p];
                p += 1;
                meta_len = (meta_len << 7) | ((b & 0x7F) as usize);
                if b & 0x80 == 0 {
                    break;
                }
            }
            p += meta_len;
            out_events.extend_from_slice(delta_bytes);
            out_events.extend_from_slice(&chunk[ev_start..p]);
            running_status = 0;
        } else if b0 == 0xF0 || b0 == 0xF7 {
            // SysEx — SKIP
            p += 1;
            let mut slen: usize = 0;
            loop {
                if p >= end {
                    break;
                }
                let b = chunk[p];
                p += 1;
                slen = (slen << 7) | ((b & 0x7F) as usize);
                if b & 0x80 == 0 {
                    break;
                }
            }
            p += slen;
            running_status = 0;
        } else {
            // MIDI channel event — SKIP (do not emit)
            if b0 & 0x80 != 0 {
                running_status = b0;
                p += 1;
            }
            let cmd = running_status & 0xF0;
            let data_bytes: usize = match cmd {
                0x80 | 0x90 | 0xA0 | 0xB0 | 0xE0 => 2,
                0xC0 | 0xD0 => 1,
                _ => 0,
            };
            p += data_bytes;
        }
    }

    // Wrap filtered events back into a valid MTrk chunk
    let mut result = Vec::with_capacity(8 + out_events.len());
    result.extend_from_slice(b"MTrk");
    let len = out_events.len() as u32;
    result.extend_from_slice(&len.to_be_bytes());
    result.extend_from_slice(&out_events);
    result
}

/// Assemble a 2-track Format-1 MIDI from a tempo chunk and one instrument chunk.
fn build_stem_midi(original: &[u8], tempo_chunk: &[u8], instrument_chunk: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(14 + tempo_chunk.len() + instrument_chunk.len());
    out.extend_from_slice(b"MThd");
    out.extend_from_slice(&[0, 0, 0, 6]); // header length = 6
    out.extend_from_slice(&[0, 1]); // format = 1 (multi-track)
    out.extend_from_slice(&[0, 2]); // 2 tracks
    // Timing division from original MThd bytes 12-13
    if original.len() >= 14 {
        out.extend_from_slice(&original[12..14]);
    } else {
        out.extend_from_slice(&[0x01, 0xE0]); // 480 ticks per quarter note
    }
    out.extend_from_slice(tempo_chunk);
    out.extend_from_slice(instrument_chunk);
    out
}

// ---------------------------------------------------------------------------
// Tool / path discovery
// ---------------------------------------------------------------------------

async fn find_sfizz_binary(config: &AppConfig) -> Option<String> {
    if let Some(path) = &config.sfizz_bin {
        return Some(path.clone());
    }
    for candidate in ["sfizz_render", "sfizz_render.exe"] {
        if Command::new(candidate)
            .arg("--help")
            .output()
            .await
            .is_ok()
        {
            return Some(candidate.to_owned());
        }
    }
    None
}

async fn find_ffmpeg_binary() -> Option<String> {
    if Command::new("ffmpeg")
        .arg("-version")
        .output()
        .await
        .is_ok()
    {
        return Some("ffmpeg".to_owned());
    }
    None
}

fn find_soundfont_dir(config: &AppConfig) -> Option<PathBuf> {
    if let Some(dir) = &config.soundfont_dir {
        if dir.exists() {
            return Some(dir.clone());
        }
    }
    // Probe common relative paths (relative to the working directory)
    for candidate in ["./VSCO-2-CE-1.1.0", "../VSCO-2-CE-1.1.0"] {
        let path = PathBuf::from(candidate);
        if path.exists() && path.join("FluteSusNV.sfz").exists() {
            return Some(path);
        }
    }
    None
}

async fn find_musescore_binary(config: &AppConfig) -> Option<String> {
    if let Some(path) = &config.musescore_bin {
        return Some(path.clone());
    }

    let candidates = [
        "MuseScoreStudio.exe",
        "MuseScore4.exe",
        "MuseScore3.exe",
        "musescore",
        "mscore",
    ];

    for candidate in candidates {
        if Command::new(candidate)
            .arg("--long-version")
            .output()
            .await
            .map(|output| output.status.success())
            .unwrap_or(false)
        {
            return Some(candidate.to_owned());
        }
    }

    None
}

// ---------------------------------------------------------------------------
// MuseScore conversion helper
// ---------------------------------------------------------------------------

async fn convert_with_musescore(
    config: &AppConfig,
    input_path: &Path,
    output_path: &Path,
    content_type: &'static str,
    extension: &'static str,
    unavailable_reason: &str,
) -> Result<ConversionOutcome> {
    let Some(binary) = find_musescore_binary(config).await else {
        return Ok(ConversionOutcome::Unavailable {
            reason: unavailable_reason.to_owned(),
        });
    };

    tracing::info!(
        "musescore: converting '{}' → {}",
        input_path.file_name().unwrap_or_default().to_string_lossy(),
        output_path.file_name().unwrap_or_default().to_string_lossy(),
    );

    let command_output = Command::new(&binary)
        .arg("-o")
        .arg(output_path)
        .arg(input_path)
        .output()
        .await
        .with_context(|| format!("failed to start MuseScore converter '{binary}'"))?;

    if !command_output.status.success() {
        let stderr = String::from_utf8_lossy(&command_output.stderr)
            .trim()
            .to_owned();
        let stdout = String::from_utf8_lossy(&command_output.stdout)
            .trim()
            .to_owned();
        let detail = if stderr.is_empty() { stdout } else { stderr };

        return Ok(ConversionOutcome::Failed {
            reason: if detail.is_empty() {
                format!("MuseScore converter '{binary}' returned a non-zero exit code.")
            } else {
                detail
            },
        });
    }

    let bytes = tokio::fs::read(output_path)
        .await
        .with_context(|| format!("failed to read generated file at {}", output_path.display()))?;

    tracing::info!(
        "musescore: done ({} KB)",
        bytes.len() / 1024,
    );

    Ok(ConversionOutcome::Ready {
        bytes: Bytes::from(bytes),
        content_type,
        extension,
    })
}

// ---------------------------------------------------------------------------
// GM program number → VSCO-2-CE SFZ filename
// ---------------------------------------------------------------------------

fn sfz_for_gm_program(program: u8, is_percussion: bool, sfz_dir: &Path) -> Option<PathBuf> {
    let filename: &str = if is_percussion {
        "GM-StylePerc.sfz"
    } else {
        match program {
            // Piano
            0..=7 => "UprightPiano.sfz",
            // Chromatic percussion
            9 => "Glockenspiel.sfz",
            12 => "Marimba.sfz",
            13 => "Xylophone.sfz",
            14 => "TubularBells.sfz",
            // Organ
            16..=23 => "OrganLoud.sfz",
            // Strings
            40 => "SViolinVib.sfz",
            41 => "ViolaEnsSusVib.sfz",
            42 => "CelloEnsSusVib.sfz",
            43 => "ContrabassSusVB.sfz",
            44 => "ViolinEnsTrem.sfz",
            45 => "SViolinPizz.sfz",
            46 => "Harp.sfz",
            47 => "Timpani.sfz",
            48..=55 => "ViolinEnsSusVib.sfz",
            // Brass
            56 => "TrumpetSus.sfz",
            57 => "TromboneSus.sfz",
            58 => "TubaSus.sfz",
            59 => "TrumpetStraightMuteSus.sfz",
            60 => "FHornSus.sfz",
            61..=63 => "FHornSus.sfz",
            // Reed
            68 => "OboeSusNV.sfz",
            69 => "OboeSusNV.sfz", // English Horn → Oboe
            70 => "BassoonSus.sfz",
            71 => "ClarinetSus.sfz",
            // Pipe
            72 => "PiccoloSus.sfz",
            73 => "FluteSusNV.sfz",
            74..=79 => "FluteSusNV.sfz",
            // Synth / unrecognised → piano fallback
            _ => "UprightPiano.sfz",
        }
    };

    let path = sfz_dir.join(filename);
    if path.exists() {
        Some(path)
    } else {
        tracing::warn!("SFZ file not found: {}", path.display());
        None
    }
}

fn gm_instrument_name(program: u8, is_percussion: bool) -> &'static str {
    if is_percussion {
        return "Percussion";
    }
    match program {
        0 => "Acoustic Piano",
        1 => "Bright Piano",
        2 => "Electric Grand Piano",
        3 => "Honky-tonk Piano",
        4 => "Electric Piano 1",
        5 => "Electric Piano 2",
        6 => "Harpsichord",
        7 => "Clavinet",
        9 => "Glockenspiel",
        10 => "Music Box",
        11 => "Vibraphone",
        12 => "Marimba",
        13 => "Xylophone",
        14 => "Tubular Bells",
        16 => "Drawbar Organ",
        17 => "Percussive Organ",
        18 => "Rock Organ",
        19 => "Church Organ",
        20 => "Reed Organ",
        40 => "Violin",
        41 => "Viola",
        42 => "Cello",
        43 => "Contrabass",
        44 => "Tremolo Strings",
        45 => "Pizzicato Strings",
        46 => "Orchestral Harp",
        47 => "Timpani",
        48 => "String Ensemble 1",
        49 => "String Ensemble 2",
        50 => "Synth Strings 1",
        51 => "Synth Strings 2",
        52 => "Choir Aahs",
        53 => "Voice Oohs",
        56 => "Trumpet",
        57 => "Trombone",
        58 => "Tuba",
        59 => "Muted Trumpet",
        60 => "French Horn",
        61 => "Brass Section",
        62 => "Synth Brass 1",
        63 => "Synth Brass 2",
        64 => "Soprano Sax",
        65 => "Alto Sax",
        66 => "Tenor Sax",
        67 => "Baritone Sax",
        68 => "Oboe",
        69 => "English Horn",
        70 => "Bassoon",
        71 => "Clarinet",
        72 => "Piccolo",
        73 => "Flute",
        74 => "Recorder",
        75 => "Pan Flute",
        76 => "Blown Bottle",
        77 => "Shakuhachi",
        78 => "Whistle",
        79 => "Ocarina",
        _ => "Instrument",
    }
}
