use crate::config::AppConfig;
use anyhow::{Context, Result};
use bytes::Bytes;
use midly::{MetaMessage, MidiMessage, Smf, Timing, TrackEventKind};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tokio::process::Command;

pub const STEM_CHUNK_DURATION_SECONDS: u32 = 4;
pub const DEFAULT_STEM_QUALITY_PROFILE: &str = "standard";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StemQualityProfile {
    Compact,
    Standard,
    High,
}

impl StemQualityProfile {
    pub fn from_slug(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().as_str() {
            "compact" => Some(Self::Compact),
            "standard" => Some(Self::Standard),
            "high" => Some(Self::High),
            _ => None,
        }
    }

    pub fn from_stored_or_default(value: &str) -> Self {
        Self::from_slug(value).unwrap_or(Self::Standard)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Standard => "standard",
            Self::High => "high",
        }
    }

    pub fn opus_bitrate(self) -> &'static str {
        match self {
            Self::Compact => "24k",
            Self::Standard => "32k",
            Self::High => "48k",
        }
    }
}

/// A `programs` entry can be either a plain SFZ path string (for
/// instruments with a single articulation) or a detail object that bundles
/// the sustain SFZ together with optional staccato, vibrato and in-track
/// program-override variants.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum ProgramEntry {
    Simple(String),
    Detailed(ProgramDetail),
}

#[derive(serde::Deserialize)]
struct ProgramDetail {
    /// Primary (sustain / default) SFZ path relative to the soundfonts dir.
    sfz: String,
    /// Short-note (<STACCATO_THRESHOLD_US) SFZ, e.g. spiccato or staccato.
    #[serde(default)]
    staccato: Option<String>,
    /// Long-note (≥VIBRATO_THRESHOLD_US) SFZ that adds natural vibrato.
    #[serde(default)]
    vibrato: Option<String>,
    /// In-track GM program-change overrides.  Key = transient program seen
    /// mid-track (e.g. "45" for pizzicato); value = SFZ path for those notes.
    #[serde(default)]
    overrides: HashMap<String, String>,
}

#[derive(serde::Deserialize)]
struct SfzMapping {
    percussion: Option<String>,
    fallback: Option<String>,
    programs: HashMap<String, ProgramEntry>,
    /// Per-soundfont gain corrections (dB) produced by the `normalize_mapping`
    /// CLI tool.  Key = relative SFZ path (forward-slash, e.g.
    /// "VSCO-2-CE-1.1.0/CelloEnsSusVib.sfz").  Absent keys → 0 dB.
    #[serde(default)]
    gains: HashMap<String, f64>,
}

async fn load_sfz_mapping(sfz_dir: &Path) -> Result<SfzMapping> {
    let path = sfz_dir.join("mapping.json");
    let text = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    let mut mapping: SfzMapping = serde_json::from_str(&text)
        .with_context(|| format!("parsing {}", path.display()))?;
    // Load gain corrections from the separate gains.json (written by
    // normalize_mapping).  Missing file is non-fatal — all gains default to 0.
    let gains_path = sfz_dir.join("gains.json");
    if gains_path.exists() {
        match tokio::fs::read_to_string(&gains_path).await {
            Ok(g) => match serde_json::from_str::<HashMap<String, f64>>(&g) {
                Ok(g) => mapping.gains = g,
                Err(e) => tracing::warn!("gains.json parse error: {e}"),
            },
            Err(e) => tracing::warn!("gains.json read error: {e}"),
        }
    }
    Ok(mapping)
}

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
    quality_profile: StemQualityProfile,
) -> Result<(Vec<StemResult>, String, Option<String>)> {
    // --- pre-flight checks ---------------------------------------------------

    tracing::info!(
        "stems: starting pipeline for '{}' with {} profile ({})",
        input_path.file_name().unwrap_or_default().to_string_lossy(),
        quality_profile.as_str(),
        quality_profile.opus_bitrate(),
    );

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

    // FluidSynth is optional — only required when mapping.json points to SF2 files.
    let fluidsynth = find_fluidsynth_binary(config).await;

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

    // --- load soundfont mapping ----------------------------------------------

    let sfz_mapping = match load_sfz_mapping(&sfz_dir).await {
        Ok(m) => m,
        Err(e) => {
            let reason = format!("Failed to load {}/mapping.json: {e}", sfz_dir.display());
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
        /// Set to the fluidsynth binary path when `sfz_path` is an SF2 file.
        fluidsynth: Option<String>,
        /// Pre-computed gain correction in dB from mapping.json `gains` table.
        /// Applied as a ffmpeg `volume` filter at encode time (0.0 = no change).
        gain_db: f64,
        /// When `Some`, this track has been split by note duration.
        /// Short notes are rendered through this staccato SFZ and mixed with
        /// the sustain stem before encoding to Opus.
        staccato_sfz_path: Option<PathBuf>,
        staccato_midi: Option<Vec<u8>>,
        staccato_mid_path: PathBuf,
        staccato_wav_path: PathBuf,
        /// Gain correction for the staccato SFZ (0.0 if not calibrated).
        staccato_gain_db: f64,
        /// When `Some`, long notes (≥ VIBRATO_THRESHOLD_US) for this track are
        /// rendered through this vibrato SFZ and mixed with the other stems.
        vibrato_sfz_path: Option<PathBuf>,
        vibrato_midi: Option<Vec<u8>>,
        vibrato_mid_path: PathBuf,
        vibrato_wav_path: PathBuf,
        /// Gain correction for the vibrato SFZ (0.0 if not calibrated).
        vibrato_gain_db: f64,
        /// Extra renders produced by in-track program changes (e.g. pizzicato
        /// sections detected via GM program 45 in a string track).  Each entry
        /// is rendered through its own SFZ patch and mixed with the main stem.
        extra_stems: Vec<ExtraStem>,
        ffmpeg: String,
    }

    /// One extra render job produced by an in-track GM program-change event.
    struct ExtraStem {
        sfz_path: PathBuf,
        midi: Vec<u8>,
        mid_path: PathBuf,
        wav_path: PathBuf,
        gain_db: f64,
    }

    let mut task_list: Vec<StemTask> = Vec::new();

    for (stem_idx, track_info) in track_infos.iter().enumerate() {
        let chunk_idx = track_info.midi_track_index;
        if chunk_idx >= chunks.len() {
            continue;
        }

        let sfz_path =
            match sfz_for_gm_program(track_info.program, track_info.is_percussion, &sfz_dir, &sfz_mapping) {
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

        // --- Program-change override split ---------------------------------
        // When a track contains in-track GM program changes (e.g. violins
        // switching to program 45 = Pizzicato Strings), extract those notes
        // into separate extra stems rendered through per-program SFZ patches.
        // The main stem canonical_mtrk only contains notes at the canonical
        // program so the wrong SFZ is never applied to the wrong notes.
        let (canonical_mtrk, extra_stems) = {
            let canon_key = track_info.program.to_string();
            let overrides = if !track_info.is_percussion {
                match sfz_mapping.programs.get(&canon_key) {
                    Some(ProgramEntry::Detailed(d)) if !d.overrides.is_empty() =>
                        Some(&d.overrides),
                    _ => None,
                }
            } else {
                None
            };
            if let Some(overrides) = overrides {
                let groups = extract_program_groups(&midi_bytes, chunk_idx);
                tracing::debug!(
                    "stems: '{}' program-groups found: {:?}",
                    track_info.track_name,
                    groups.keys().collect::<Vec<_>>()
                );
                let canon_events = groups.get(&track_info.program)
                    .cloned()
                    .unwrap_or_default();
                // Fall back to the raw chunk only when parsing failed entirely
                // (groups is empty).  When groups is non-empty but canon_events
                // is empty the track has *only* override-program notes (e.g.
                // pure pizzicato cello) — the correct canon track is silent so
                // the extra stems handle all playback.
                let canon_mtrk = if groups.is_empty() {
                    chunks[chunk_idx].to_vec()
                } else {
                    build_mtrk(canon_events)
                };
                let mut extras: Vec<ExtraStem> = Vec::new();
                for (prog, events) in &groups {
                    if *prog == track_info.program || events.is_empty() { continue; }
                    let prog_key = prog.to_string();
                    if let Some(sfz_rel) = overrides.get(&prog_key) {
                        let sfz_p = sfz_dir.join(sfz_rel);
                        if sfz_p.exists() {
                            let extra_mtrk = build_mtrk(events.clone());
                            let extra_midi = build_stem_midi(
                                &midi_bytes, &clean_tempo_chunk, &extra_mtrk
                            );
                            let gain = sfz_p
                                .strip_prefix(&sfz_dir).ok()
                                .map(|r| r.to_string_lossy().replace('\\', "/"))
                                .and_then(|r| sfz_mapping.gains.get(&r).copied())
                                .unwrap_or(0.0);
                            let n = extras.len();
                            extras.push(ExtraStem {
                                sfz_path: sfz_p,
                                midi: extra_midi,
                                mid_path: output_dir.join(format!("stem_{chunk_idx}_x{n}.mid")),
                                wav_path: output_dir.join(format!("stem_{chunk_idx}_x{n}.wav")),
                                gain_db: gain,
                            });
                        } else {
                            tracing::warn!("Program override SFZ not found: {}",
                                sfz_dir.join(sfz_rel).display());
                        }
                    }
                }
                (canon_mtrk, extras)
            } else {
                (chunks[chunk_idx].to_vec(), Vec::new())
            }
        };

        // --- Note-duration split -------------------------------------------
        // Notes are classified by sounding duration and routed to dedicated SFZ
        // patches to capture natural articulation:
        //   < STACCATO_THRESHOLD_US  → staccato / spiccato patch
        //   ≥ VIBRATO_THRESHOLD_US   → vibrato sustain patch
        //   everything in between    → plain (non-vibrato) sustain patch
        // The split operates on the canonical_mtrk (program-filtered events)
        // so pizzicato notes that were already extracted above are not also
        // misrouted to the spiccato patch.
        let staccato_sfz = if !track_info.is_percussion {
            staccato_sfz_for_gm_program(track_info.program, &sfz_dir, &sfz_mapping)
        } else {
            None
        };
        let vibrato_sfz = if !track_info.is_percussion {
            vibrato_sfz_for_gm_program(track_info.program, &sfz_dir, &sfz_mapping)
        } else {
            None
        };

        let (stem_midi, staccato_midi, vibrato_midi) =
            if staccato_sfz.is_some() || vibrato_sfz.is_some() {
                // Build a 2-track MIDI from the canonical (filtered) events
                // so split_midi_track_3way can be called with track_idx = 1.
                let split_input = build_stem_midi(
                    &midi_bytes, &clean_tempo_chunk, &canonical_mtrk
                );
                let (stac_chunk, sus_chunk, vib_chunk) = split_midi_track_3way(
                    &split_input,
                    1,
                    staccato_sfz.is_some(),
                    vibrato_sfz.is_some(),
                );
                let sus  = build_stem_midi(&midi_bytes, &clean_tempo_chunk, &sus_chunk);
                let stac = staccato_sfz.is_some()
                    .then(|| build_stem_midi(&midi_bytes, &clean_tempo_chunk, &stac_chunk));
                let vib  = vibrato_sfz.is_some()
                    .then(|| build_stem_midi(&midi_bytes, &clean_tempo_chunk, &vib_chunk));
                (sus, stac, vib)
            } else {
                (build_stem_midi(&midi_bytes, &clean_tempo_chunk, &canonical_mtrk), None, None)
            };

        // Look up the per-instrument gain correction from mapping.json.
        // The key uses forward slashes regardless of OS.
        let gain_db = sfz_path
            .strip_prefix(&sfz_dir)
            .ok()
            .map(|rel| rel.to_string_lossy().replace('\\', "/"))
            .and_then(|rel| sfz_mapping.gains.get(&rel).copied())
            .unwrap_or(0.0);

        let staccato_gain_db = staccato_sfz.as_ref()
            .and_then(|p| p.strip_prefix(&sfz_dir).ok())
            .map(|rel| rel.to_string_lossy().replace('\\', "/"))
            .and_then(|rel| sfz_mapping.gains.get(&rel).copied())
            .unwrap_or(0.0);

        let vibrato_gain_db = vibrato_sfz.as_ref()
            .and_then(|p| p.strip_prefix(&sfz_dir).ok())
            .map(|rel| rel.to_string_lossy().replace('\\', "/"))
            .and_then(|rel| sfz_mapping.gains.get(&rel).copied())
            .unwrap_or(0.0);

        task_list.push(StemTask {
            stem_idx,
            chunk_idx,
            track_name: track_info.track_name.clone(),
            program: track_info.program,
            is_percussion: track_info.is_percussion,
            sfz_path: sfz_path.clone(),
            stem_midi,
            stem_mid_path: output_dir.join(format!("stem_{chunk_idx}.mid")),
            stem_wav_path: output_dir.join(format!("stem_{chunk_idx}.wav")),
            stem_ogg_path: output_dir.join(format!("stem_{chunk_idx}.ogg")),
            sfizz: sfizz.clone(),
            fluidsynth: if sfz_path.extension().map_or(false, |e| e.eq_ignore_ascii_case("sf2")) {
                fluidsynth.clone()
            } else {
                None
            },
            gain_db,
            staccato_sfz_path: staccato_sfz,
            staccato_midi,
            staccato_mid_path: output_dir.join(format!("stem_{chunk_idx}_stac.mid")),
            staccato_wav_path: output_dir.join(format!("stem_{chunk_idx}_stac.wav")),
            staccato_gain_db,
            vibrato_sfz_path: vibrato_sfz,
            vibrato_midi,
            vibrato_mid_path: output_dir.join(format!("stem_{chunk_idx}_vib.mid")),
            vibrato_wav_path: output_dir.join(format!("stem_{chunk_idx}_vib.wav")),
            vibrato_gain_db,
            extra_stems,
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

            // Render WAV: use FluidSynth for SF2 soundfonts, sfizz for SFZ.
            // Use 48000 Hz to match Opus's native sample rate — ffmpeg will
            // then pass the samples through without resampling, eliminating
            // a source of inter-stem timing drift.
            let is_sf2 = task.sfz_path.extension()
                .map_or(false, |e| e.eq_ignore_ascii_case("sf2"));

            if is_sf2 {
                let fluidsynth_bin = match &task.fluidsynth {
                    Some(bin) => bin.clone(),
                    None => {
                        tracing::warn!(
                            "stems: [{}/{}] '{}' – SF2 soundfont requires FluidSynth but it \
                            was not found. Install FluidSynth and add it to PATH (or set \
                            FLUIDSYNTH_BIN).",
                            task.stem_idx + 1,
                            total,
                            task.track_name,
                        );
                        return None;
                    }
                };
                match Command::new(&fluidsynth_bin)
                    .arg("-ni")
                    .arg("-q")
                    .arg("-r").arg("48000")
                    .arg("-F").arg(&task.stem_wav_path)
                    .arg(&task.sfz_path)
                    .arg(&task.stem_mid_path)
                    .output()
                    .await
                {
                    Ok(out) if out.status.success() => {}
                    Ok(out) => {
                        tracing::warn!(
                            "stems: [{}/{}] '{}' – FluidSynth failed: {}",
                            task.stem_idx + 1,
                            total,
                            task.track_name,
                            String::from_utf8_lossy(&out.stderr).trim()
                        );
                        return None;
                    }
                    Err(e) => {
                        tracing::warn!(
                            "stems: [{}/{}] '{}' – FluidSynth spawn error: {e}",
                            task.stem_idx + 1,
                            total,
                            task.track_name,
                        );
                        return None;
                    }
                }
            } else {
                match Command::new(&task.sfizz)
                    .arg("--sfz")
                    .arg(&task.sfz_path)
                    .arg("--midi")
                    .arg(&task.stem_mid_path)
                    .arg("--wav")
                    .arg(&task.stem_wav_path)
                    .arg("--samplerate")
                    .arg("48000")
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
            }

            // --- Optional staccato render ------------------------------------
            // When this track was split by note duration, render the staccato
            // (short-note) MIDI through the staccato SFZ patch.
            let staccato_wave_ok =
                if let (Some(stac_sfz), Some(stac_midi)) =
                    (&task.staccato_sfz_path, &task.staccato_midi)
                {
                    match tokio::fs::write(&task.staccato_mid_path, stac_midi).await {
                        Err(e) => {
                            tracing::warn!(
                                "stems: [{}/{}] '{}' – staccato MIDI write: {e}",
                                task.stem_idx + 1, total, task.track_name
                            );
                            false
                        }
                        Ok(()) => {
                            match Command::new(&task.sfizz)
                                .arg("--sfz").arg(stac_sfz)
                                .arg("--midi").arg(&task.staccato_mid_path)
                                .arg("--wav").arg(&task.staccato_wav_path)
                                .arg("--samplerate").arg("48000")
                                .output().await
                            {
                                Ok(o) if o.status.success() => {
                                    tracing::info!(
                                        "stems: [{}/{}] '{}' – staccato render OK",
                                        task.stem_idx + 1, total, task.track_name
                                    );
                                    true
                                }
                                Ok(o) => {
                                    tracing::warn!(
                                        "stems: [{}/{}] '{}' – staccato sfizz: {}",
                                        task.stem_idx + 1, total, task.track_name,
                                        String::from_utf8_lossy(&o.stderr).trim()
                                    );
                                    false
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "stems: [{}/{}] '{}' – staccato sfizz spawn: {e}",
                                        task.stem_idx + 1, total, task.track_name
                                    );
                                    false
                                }
                            }
                        }
                    }
                } else {
                    false
                };

            // --- Optional vibrato render -------------------------------------
            // Long notes (≥ VIBRATO_THRESHOLD_US) rendered through the vibrato
            // SFZ patch and mixed into the stem alongside sustain + staccato.
            let vibrato_wave_ok =
                if let (Some(vib_sfz), Some(vib_midi)) =
                    (&task.vibrato_sfz_path, &task.vibrato_midi)
                {
                    match tokio::fs::write(&task.vibrato_mid_path, vib_midi).await {
                        Err(e) => {
                            tracing::warn!(
                                "stems: [{}/{}] '{}' – vibrato MIDI write: {e}",
                                task.stem_idx + 1, total, task.track_name
                            );
                            false
                        }
                        Ok(()) => {
                            match Command::new(&task.sfizz)
                                .arg("--sfz").arg(vib_sfz)
                                .arg("--midi").arg(&task.vibrato_mid_path)
                                .arg("--wav").arg(&task.vibrato_wav_path)
                                .arg("--samplerate").arg("48000")
                                .output().await
                            {
                                Ok(o) if o.status.success() => {
                                    tracing::info!(
                                        "stems: [{}/{}] '{}' – vibrato render OK",
                                        task.stem_idx + 1, total, task.track_name
                                    );
                                    true
                                }
                                Ok(o) => {
                                    tracing::warn!(
                                        "stems: [{}/{}] '{}' – vibrato sfizz: {}",
                                        task.stem_idx + 1, total, task.track_name,
                                        String::from_utf8_lossy(&o.stderr).trim()
                                    );
                                    false
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "stems: [{}/{}] '{}' – vibrato sfizz spawn: {e}",
                                        task.stem_idx + 1, total, task.track_name
                                    );
                                    false
                                }
                            }
                        }
                    }
                } else {
                    false
                };

            // --- Extra stems (program-change overrides, e.g. pizzicato) ----
            let mut extra_wav_ok: Vec<(PathBuf, f64)> = Vec::new();
            for extra in &task.extra_stems {
                if let Err(e) = tokio::fs::write(&extra.mid_path, &extra.midi).await {
                    tracing::warn!(
                        "stems: [{}/{}] '{}' – extra stem MIDI write: {e}",
                        task.stem_idx + 1, total, task.track_name
                    );
                    continue;
                }
                match Command::new(&task.sfizz)
                    .arg("--sfz").arg(&extra.sfz_path)
                    .arg("--midi").arg(&extra.mid_path)
                    .arg("--wav").arg(&extra.wav_path)
                    .arg("--samplerate").arg("48000")
                    .output().await
                {
                    Ok(o) if o.status.success() => {
                        tracing::info!(
                            "stems: [{}/{}] '{}' – extra stem OK ({})",
                            task.stem_idx + 1, total, task.track_name,
                            extra.sfz_path.file_name().unwrap_or_default().to_string_lossy()
                        );
                        extra_wav_ok.push((extra.wav_path.clone(), extra.gain_db));
                    }
                    Ok(o) => tracing::warn!(
                        "stems: [{}/{}] '{}' – extra stem sfizz ({}): {}",
                        task.stem_idx + 1, total, task.track_name,
                        extra.sfz_path.file_name().unwrap_or_default().to_string_lossy(),
                        String::from_utf8_lossy(&o.stderr).trim()
                    ),
                    Err(e) => tracing::warn!(
                        "stems: [{}/{}] '{}' – extra stem sfizz spawn: {e}",
                        task.stem_idx + 1, total, task.track_name
                    ),
                }
            }

            // Encode WAV → Opus.  Collect all stems that rendered successfully
            // (sustain always present; staccato, vibrato, extra optional), apply
            // per-SFZ gain correction, and mix in a single ffmpeg pass.
            let mut sources: Vec<(&PathBuf, f64)> =
                vec![(&task.stem_wav_path, task.gain_db)];
            if staccato_wave_ok {
                sources.push((&task.staccato_wav_path, task.staccato_gain_db));
            }
            if vibrato_wave_ok {
                sources.push((&task.vibrato_wav_path, task.vibrato_gain_db));
            }
            for (wav, gain) in &extra_wav_ok {
                sources.push((wav, *gain));
            }
            let mut ffmpeg_cmd = Command::new(&task.ffmpeg);
            ffmpeg_cmd.arg("-y");
            for (path, _) in &sources {
                ffmpeg_cmd.arg("-i").arg(*path);
            }
            if sources.len() == 1 {
                if sources[0].1.abs() > 0.05 {
                    ffmpeg_cmd.arg("-af").arg(format!("volume={:.2}dB", sources[0].1));
                }
            } else {
                let n = sources.len();
                let mut filter = String::new();
                for (i, (_, gain)) in sources.iter().enumerate() {
                    filter.push_str(&format!("[{i}:a]volume={:.2}dB[a{i}];", gain));
                }
                let inputs: String = (0..n).map(|i| format!("[a{i}]")).collect();
                filter.push_str(&format!("{}amix=inputs={n}:normalize=0[aout]", inputs));
                ffmpeg_cmd.arg("-filter_complex").arg(&filter).arg("-map").arg("[aout]");
            }
            ffmpeg_cmd
                .arg("-c:a")
                .arg("libopus")
                .arg("-b:a")
                .arg(quality_profile.opus_bitrate())
                .arg("-ac")
                .arg("1")
                .arg("-application")
                .arg("audio")
                .arg("-ar")
                .arg("48000")
                .arg(&task.stem_ogg_path);
            match ffmpeg_cmd.output().await
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

async fn find_fluidsynth_binary(config: &AppConfig) -> Option<String> {
    if let Some(path) = &config.fluidsynth_bin {
        return Some(path.clone());
    }
    for candidate in ["fluidsynth", "fluidsynth.exe"] {
        if Command::new(candidate)
            .arg("--version")
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
    // Probe common relative paths (relative to the working directory).
    // The soundfonts root is identified by the presence of mapping.json.
    for candidate in ["./soundfonts", "../soundfonts"] {
        let path = PathBuf::from(candidate);
        if path.exists() && path.join("mapping.json").exists() {
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

    let xdg_runtime_dir = std::env::temp_dir().join("fumen-musescore-runtime");
    tokio::fs::create_dir_all(&xdg_runtime_dir)
        .await
        .with_context(|| format!("failed to create {}", xdg_runtime_dir.display()))?;

    let command_output = Command::new(&binary)
        .env("QT_QPA_PLATFORM", "offscreen")
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("XDG_RUNTIME_DIR", &xdg_runtime_dir)
        .arg("-o")
        .arg(output_path)
        .arg(input_path)
        .output()
        .await
        .with_context(|| format!("failed to start MuseScore converter '{binary}'"))?;

    if !command_output.status.success() {
        let stderr = sanitize_musescore_output(String::from_utf8_lossy(&command_output.stderr).as_ref());
        let stdout = sanitize_musescore_output(String::from_utf8_lossy(&command_output.stdout).as_ref());
        let status = command_output
            .status
            .code()
            .map(|code| format!("exit code {code}"))
            .unwrap_or_else(|| "terminated by signal".to_owned());
        let detail = match (stdout.is_empty(), stderr.is_empty()) {
            (false, false) => format!("stdout:\n{stdout}\nstderr:\n{stderr}"),
            (false, true) => stdout,
            (true, false) => stderr,
            (true, true) => String::new(),
        };

        return Ok(ConversionOutcome::Failed {
            reason: if detail.is_empty() {
                format!("MuseScore converter '{binary}' failed with {status}.")
            } else {
                format!("MuseScore converter '{binary}' failed with {status}.\n{detail}")
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

fn sanitize_musescore_output(output: &str) -> String {
    output
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty()
                && trimmed != "/lib/x86_64-linux-gnu/libOpenGL.so.0"
                && trimmed != "/lib/x86_64-linux-gnu/libjack.so.0"
                && trimmed != "/lib/x86_64-linux-gnu/libnss3.so"
                && trimmed
                    != "findlib: libpipewire-0.3.so.0: cannot open shared object file: No such file or directory"
                && trimmed != "/opt/musescore4/AppRun: Using fallback for library 'libpipewire-0.3.so.0'"
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// GM program number → VSCO-2-CE SFZ filename
// ---------------------------------------------------------------------------

fn sfz_for_gm_program(
    program: u8,
    is_percussion: bool,
    sfz_dir: &Path,
    mapping: &SfzMapping,
) -> Option<PathBuf> {
    let rel: &str = if is_percussion {
        mapping.percussion.as_deref().unwrap_or("VSCO-2-CE-1.1.0/GM-StylePerc.sfz")
    } else {
        let key = program.to_string();
        match mapping.programs.get(&key) {
            Some(ProgramEntry::Simple(p)) => p.as_str(),
            Some(ProgramEntry::Detailed(d)) => d.sfz.as_str(),
            None => mapping.fallback.as_deref().unwrap_or("VSCO-2-CE-1.1.0/UprightPiano.sfz"),
        }
    };

    let path = sfz_dir.join(rel);
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

// ---------------------------------------------------------------------------
// Articulation note-duration splitting
// ---------------------------------------------------------------------------

/// Notes shorter than this wall-clock duration (µs) use the staccato SFZ.
/// 200 ms covers typical staccato marks at any orchestral tempo.
const STACCATO_THRESHOLD_US: u64 = 200_000;

/// Notes sustaining for at least this wall-clock duration (µs) use the
/// vibrato SFZ when one is configured.  800 ms ≈ a half note at 75 BPM,
/// which captures most consciously held notes in an orchestral score.
const VIBRATO_THRESHOLD_US:  u64 = 800_000;

/// Return the staccato SFZ path configured for a GM program number, or `None`
/// if the program entry has no staccato variant.
fn staccato_sfz_for_gm_program(
    program: u8,
    sfz_dir: &Path,
    mapping: &SfzMapping,
) -> Option<PathBuf> {
    let rel = match mapping.programs.get(&program.to_string())? {
        ProgramEntry::Detailed(d) => d.staccato.as_deref()?,
        _ => return None,
    };
    let path = sfz_dir.join(rel);
    if path.exists() {
        Some(path)
    } else {
        tracing::warn!("Staccato SFZ not found: {}", path.display());
        None
    }
}

/// Return the vibrato SFZ path configured for a GM program number, or `None`
/// if the program entry has no vibrato variant.
fn vibrato_sfz_for_gm_program(
    program: u8,
    sfz_dir: &Path,
    mapping: &SfzMapping,
) -> Option<PathBuf> {
    let rel = match mapping.programs.get(&program.to_string())? {
        ProgramEntry::Detailed(d) => d.vibrato.as_deref()?,
        _ => return None,
    };
    let path = sfz_dir.join(rel);
    if path.exists() {
        Some(path)
    } else {
        tracing::warn!("Vibrato SFZ not found: {}", path.display());
        None
    }
}

/// Split one MIDI track's note events into three MTrk chunks based on
/// sounding duration:
///
/// - `stac_chunk`: notes shorter than `STACCATO_THRESHOLD_US`  (only when
///   `has_staccato` is true; otherwise those notes go to `sus_chunk`)
/// - `vib_chunk`:  notes at least `VIBRATO_THRESHOLD_US` long  (only when
///   `has_vibrato` is true; otherwise those notes go to `sus_chunk`)
/// - `sus_chunk`:  everything in between, plus any notes not split above
///
/// All non-note MIDI events are duplicated into every chunk so each can be
/// rendered independently by sfizz.  Returns `(stac_chunk, sus_chunk, vib_chunk)`.
fn split_midi_track_3way(
    midi_bytes: &[u8],
    track_idx: usize,
    has_staccato: bool,
    has_vibrato: bool,
) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let smf = match Smf::parse(midi_bytes) {
        Ok(s) => s,
        Err(_) => return (Vec::new(), Vec::new(), Vec::new()),
    };

    let ticks_per_qn: u32 = match smf.header.timing {
        Timing::Metrical(t) => u16::from(t) as u32,
        _ => 480,
    };

    let Some(track) = smf.tracks.get(track_idx) else {
        return (Vec::new(), Vec::new(), Vec::new());
    };

    let tempo_map = match smf.tracks.first() {
        Some(t) => build_tempo_map(t),
        None => vec![(0u32, 500_000u32)],
    };

    // --- Pass 1: compute absolute ticks and classify each NoteOn ------------

    let mut abs_ticks: Vec<u32> = Vec::with_capacity(track.len());
    {
        let mut abs = 0u32;
        for ev in track.iter() {
            abs = abs.saturating_add(u32::from(ev.delta));
            abs_ticks.push(abs);
        }
    }

    let mut active: HashMap<(u8, u8), u32> = HashMap::new();
    let mut staccato_set: HashSet<(u8, u8, u32)> = HashSet::new();
    let mut vibrato_set:  HashSet<(u8, u8, u32)> = HashSet::new();

    for (i, ev) in track.iter().enumerate() {
        let tick = abs_ticks[i];
        let TrackEventKind::Midi { channel, message } = &ev.kind else { continue };
        let ch = u8::from(*channel);
        match message {
            MidiMessage::NoteOn { key, vel } if u8::from(*vel) > 0 => {
                active.insert((ch, u8::from(*key)), tick);
            }
            MidiMessage::NoteOff { key, .. } | MidiMessage::NoteOn { key, .. } => {
                let note = u8::from(*key);
                if let Some(on_tick) = active.remove(&(ch, note)) {
                    let dur_us = ticks_to_us(
                        on_tick,
                        tick.saturating_sub(on_tick),
                        &tempo_map,
                        ticks_per_qn,
                    );
                    if has_staccato && dur_us < STACCATO_THRESHOLD_US {
                        staccato_set.insert((ch, note, on_tick));
                    } else if has_vibrato && dur_us >= VIBRATO_THRESHOLD_US {
                        vibrato_set.insert((ch, note, on_tick));
                    }
                    // else → sustain (default / middle tier)
                }
            }
            _ => {}
        }
    }

    // --- Pass 2: route events into staccato / sustain / vibrato streams -----

    let mut open_stac: HashSet<(u8, u8)> = HashSet::new();
    let mut open_vib:  HashSet<(u8, u8)> = HashSet::new();
    let mut stac_events: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut sus_events:  Vec<(u32, Vec<u8>)> = Vec::new();
    let mut vib_events:  Vec<(u32, Vec<u8>)> = Vec::new();

    let mut abs = 0u32;
    for ev in track.iter() {
        abs = abs.saturating_add(u32::from(ev.delta));
        let tick = abs;
        let TrackEventKind::Midi { channel, message } = &ev.kind else { continue };
        let ch = u8::from(*channel);
        let encoded = encode_midi_event(ch, message);
        if encoded.is_empty() {
            continue;
        }
        match message {
            MidiMessage::NoteOn { key, vel } if u8::from(*vel) > 0 => {
                let note = u8::from(*key);
                if staccato_set.contains(&(ch, note, tick)) {
                    open_stac.insert((ch, note));
                    stac_events.push((tick, encoded));
                } else if vibrato_set.contains(&(ch, note, tick)) {
                    open_vib.insert((ch, note));
                    vib_events.push((tick, encoded));
                } else {
                    sus_events.push((tick, encoded));
                }
            }
            MidiMessage::NoteOff { key, .. } | MidiMessage::NoteOn { key, .. } => {
                let note = u8::from(*key);
                if open_stac.remove(&(ch, note)) {
                    stac_events.push((tick, encoded));
                } else if open_vib.remove(&(ch, note)) {
                    vib_events.push((tick, encoded));
                } else {
                    sus_events.push((tick, encoded));
                }
            }
            _ => {
                // Non-note events → all chunks so each is self-contained.
                stac_events.push((tick, encoded.clone()));
                vib_events.push((tick, encoded.clone()));
                sus_events.push((tick, encoded));
            }
        }
    }

    (build_mtrk(stac_events), build_mtrk(sus_events), build_mtrk(vib_events))
}

/// Collect tempo change events from a MIDI track into a sorted
/// `Vec<(abs_tick, µs_per_quarter_note)>`.  Defaults to 120 BPM (500 000 µs).
fn build_tempo_map(track: &[midly::TrackEvent<'_>]) -> Vec<(u32, u32)> {
    let mut map: Vec<(u32, u32)> = vec![(0, 500_000)];
    let mut abs = 0u32;
    for ev in track {
        abs = abs.saturating_add(u32::from(ev.delta));
        if let TrackEventKind::Meta(MetaMessage::Tempo(t)) = &ev.kind {
            let us = u32::from(*t);
            if let Some(last) = map.last_mut() {
                if last.0 == abs {
                    last.1 = us;
                    continue;
                }
            }
            map.push((abs, us));
        }
    }
    map
}

/// Convert a tick-based note duration to wall-clock microseconds, correctly
/// handling tempo changes within the note's span.
fn ticks_to_us(
    start_tick: u32,
    duration_ticks: u32,
    tempo_map: &[(u32, u32)],
    ticks_per_qn: u32,
) -> u64 {
    if duration_ticks == 0 || ticks_per_qn == 0 {
        return 0;
    }
    let end_tick = start_tick.saturating_add(duration_ticks);
    let mut us = 0u64;
    let mut cursor = start_tick;
    for i in 0..tempo_map.len() {
        let seg_start = tempo_map[i].0;
        let seg_tempo = tempo_map[i].1 as u64;
        let seg_end   = tempo_map.get(i + 1).map(|t| t.0).unwrap_or(u32::MAX);
        if seg_end  <= cursor    { continue; }
        if seg_start >= end_tick { break;    }
        let ticks = (end_tick.min(seg_end) - cursor.max(seg_start)) as u64;
        us += ticks * seg_tempo / ticks_per_qn as u64;
        cursor = end_tick.min(seg_end);
        if cursor >= end_tick { break; }
    }
    us
}

/// Encode a single MIDI channel-voice event to raw bytes (no delta prefix).
/// Returns an empty `Vec` for event types that are intentionally skipped.
fn encode_midi_event(channel: u8, message: &MidiMessage) -> Vec<u8> {
    match message {
        MidiMessage::NoteOn  { key, vel } =>
            vec![0x90 | channel, u8::from(*key), u8::from(*vel)],
        MidiMessage::NoteOff { key, vel } =>
            vec![0x80 | channel, u8::from(*key), u8::from(*vel)],
        MidiMessage::Controller { controller, value } =>
            vec![0xB0 | channel, u8::from(*controller), u8::from(*value)],
        MidiMessage::ProgramChange { program } =>
            vec![0xC0 | channel, u8::from(*program)],
        MidiMessage::Aftertouch { key, vel } =>
            vec![0xA0 | channel, u8::from(*key), u8::from(*vel)],
        MidiMessage::ChannelAftertouch { vel } =>
            vec![0xD0 | channel, u8::from(*vel)],
        MidiMessage::PitchBend { bend } => {
            // bend.0 is the raw u14 value: 0x0000=min, 0x2000=center, 0x3FFF=max
            let raw = u16::from(bend.0);
            vec![0xE0 | channel, (raw & 0x7F) as u8, ((raw >> 7) & 0x7F) as u8]
        }
    }
}

/// Pack a sorted list of `(abs_tick, event_bytes)` pairs into a valid MTrk
/// chunk.  Delta times are re-computed from the absolute-tick values.
/// An EndOfTrack meta event is appended automatically.
fn build_mtrk(events: Vec<(u32, Vec<u8>)>) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    let mut prev = 0u32;
    for (tick, ev) in &events {
        vlq_write(&mut body, tick.saturating_sub(prev));
        body.extend_from_slice(ev);
        prev = *tick;
    }
    vlq_write(&mut body, 0);
    body.extend_from_slice(&[0xFF, 0x2F, 0x00]); // EndOfTrack
    let mut chunk = Vec::with_capacity(8 + body.len());
    chunk.extend_from_slice(b"MTrk");
    chunk.extend_from_slice(&(body.len() as u32).to_be_bytes());
    chunk.extend_from_slice(&body);
    chunk
}

/// MIDI variable-length quantity (VLQ) encoder.
fn vlq_write(buf: &mut Vec<u8>, v: u32) {
    if v < 0x80 {
        buf.push(v as u8);
        return;
    }
    let mut b = [0u8; 4];
    let mut n = 0usize;
    let mut r = v;
    while r > 0 {
        b[n] = (r & 0x7F) as u8;
        n += 1;
        r >>= 7;
    }
    for i in (0..n).rev() {
        buf.push(if i > 0 { b[i] | 0x80 } else { b[i] });
    }
}

// ---------------------------------------------------------------------------
// In-track program-change grouping
// ---------------------------------------------------------------------------

/// Walk a MIDI instrument track and return note events grouped by the GM
/// program number that was active when each note started.
///
/// Non-note MIDI events (controllers, pitch bend, aftertouch) are duplicated
/// into every group that contains at least one note so each group can be
/// rendered independently by sfizz.  Program-change events themselves are
/// discarded — sfizz uses the SFZ file, not GM program numbers.
///
/// Returns a `HashMap<program, sorted_abs_tick_events>`.
fn extract_program_groups(
    midi_bytes: &[u8],
    track_idx: usize,
) -> HashMap<u8, Vec<(u32, Vec<u8>)>> {
    let smf = match Smf::parse(midi_bytes) {
        Ok(s) => s,
        Err(_) => return HashMap::new(),
    };
    let Some(track) = smf.tracks.get(track_idx) else {
        return HashMap::new();
    };

    // Per-channel current program (MuseScore encodes articulations as separate
    // channels within the same track, each with its own ProgramChange at t=0).
    let mut current_program: [u8; 16] = [0u8; 16];
    let mut groups: HashMap<u8, Vec<(u32, Vec<u8>)>> = HashMap::new();
    // Tracks which program each open note belongs to so NoteOff goes to
    // the same group as its paired NoteOn.
    let mut open_notes: HashMap<(u8, u8), u8> = HashMap::new();
    // Non-note events collected for later duplication into all groups.
    let mut shared: Vec<(u32, Vec<u8>)> = Vec::new();

    let mut abs = 0u32;
    for ev in track.iter() {
        abs = abs.saturating_add(u32::from(ev.delta));
        let tick = abs;
        let TrackEventKind::Midi { channel, message } = &ev.kind else { continue };
        let ch = u8::from(*channel);
        match message {
            MidiMessage::ProgramChange { program } => {
                current_program[ch as usize] = u8::from(*program);
                // Not forwarded to sfizz — sfizz uses the --sfz file directly.
            }
            MidiMessage::NoteOn { key, vel } if u8::from(*vel) > 0 => {
                let note = u8::from(*key);
                let prog = current_program[ch as usize];
                open_notes.insert((ch, note), prog);
                let enc = encode_midi_event(ch, message);
                groups.entry(prog).or_default().push((tick, enc));
            }
            MidiMessage::NoteOff { key, .. } | MidiMessage::NoteOn { key, .. } => {
                let note = u8::from(*key);
                let prog = open_notes.remove(&(ch, note)).unwrap_or(current_program[ch as usize]);
                let enc = encode_midi_event(ch, message);
                groups.entry(prog).or_default().push((tick, enc));
            }
            _ => {
                let enc = encode_midi_event(ch, message);
                if !enc.is_empty() {
                    shared.push((tick, enc));
                }
            }
        }
    }

    // Duplicate shared (non-note) events into every group and re-sort by tick.
    if !shared.is_empty() {
        for events in groups.values_mut() {
            for (tick, enc) in &shared {
                events.push((*tick, enc.clone()));
            }
            events.sort_by_key(|(t, _)| *t);
        }
    }

    groups
}
