use crate::config::AppConfig;
use crate::models::DrumMapEntry;
use anyhow::{Context, Result};
use bytes::Bytes;
use midly::{MetaMessage, MidiMessage, Smf, Timing, TrackEventKind};
use roxmltree::{Document, Node, ParsingOptions};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use tokio::process::Command;
use zip::ZipArchive;

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
    /// Gain corrections loaded from `gains.json`.
    /// Supports path aliases and semantic program ids such as `42`,
    /// `42.staccato`, `42.vibrato`, `42.override.45`, `percussion`, and
    /// `fallback`.
    #[serde(default)]
    gains: HashMap<String, f64>,
}

async fn load_sfz_mapping(sfz_dir: &Path) -> Result<SfzMapping> {
    let path = sfz_dir.join("mapping.json");
    let text = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    let mut mapping: SfzMapping =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
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

fn gain_path_key_for_soundfont(path: &Path, sfz_dir: &Path) -> Option<String> {
    path.strip_prefix(sfz_dir)
        .ok()
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
}

fn lookup_gain(mapping: &SfzMapping, rel_path: Option<&str>, semantic_keys: &[&str]) -> f64 {
    for key in semantic_keys {
        if let Some(gain) = mapping.gains.get(*key) {
            return *gain;
        }
    }

    if let Some(rel_path) = rel_path {
        if let Some(gain) = mapping.gains.get(rel_path) {
            return *gain;
        }

        if let Some(legacy_rel_path) = rel_path.strip_prefix("data/") {
            if let Some(gain) = mapping.gains.get(legacy_rel_path) {
                return *gain;
            }
        }
    }

    0.0
}

fn extract_musescore_drumset_mappings(score_path: &Path) -> Result<DrumsetMappingMap> {
    if !score_path
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| ext.eq_ignore_ascii_case("mscz"))
    {
        return Ok(HashMap::new());
    }

    let file = File::open(score_path)
        .with_context(|| format!("opening MuseScore archive {}", score_path.display()))?;
    let mut archive =
        ZipArchive::new(file).with_context(|| format!("reading {}", score_path.display()))?;

    let mscx_name = (0..archive.len())
        .filter_map(|idx| {
            archive
                .by_index(idx)
                .ok()
                .map(|entry| entry.name().to_owned())
        })
        .find(|name| name.ends_with(".mscx") && !name.contains('/'))
        .or_else(|| {
            (0..archive.len())
                .filter_map(|idx| {
                    archive
                        .by_index(idx)
                        .ok()
                        .map(|entry| entry.name().to_owned())
                })
                .find(|name| name.ends_with(".mscx") && !name.starts_with("Excerpts/"))
        });

    let Some(mscx_name) = mscx_name else {
        return Ok(HashMap::new());
    };

    let mut entry = archive
        .by_name(&mscx_name)
        .with_context(|| format!("opening '{mscx_name}' in {}", score_path.display()))?;
    let mut xml = String::new();
    entry
        .read_to_string(&mut xml)
        .with_context(|| format!("reading '{mscx_name}' in {}", score_path.display()))?;

    let document = Document::parse(&xml)
        .with_context(|| format!("parsing '{mscx_name}' in {}", score_path.display()))?;

    let mut mappings = HashMap::new();
    for part in document
        .descendants()
        .filter(|node| node.has_tag_name("Part"))
    {
        let Some(instrument) = part
            .children()
            .find(|child| child.is_element() && child.has_tag_name("Instrument"))
        else {
            continue;
        };

        let uses_drumset = instrument
            .children()
            .find(|child| child.is_element() && child.has_tag_name("useDrumset"))
            .and_then(|node| node.text())
            .is_some_and(|value| value.trim() == "1");
        if !uses_drumset {
            continue;
        }

        let drum_map: Vec<DrumMapEntry> = instrument
            .children()
            .filter(|child| child.is_element() && child.has_tag_name("Drum"))
            .filter_map(parse_musescore_drum_entry)
            .collect();
        if drum_map.is_empty() {
            continue;
        }

        let aliases = musescore_part_name_aliases(part, instrument);
        for alias in aliases {
            mappings.entry(alias).or_insert_with(|| drum_map.clone());
        }
    }

    Ok(mappings)
}

fn parse_musescore_drum_entry(drum: Node<'_, '_>) -> Option<DrumMapEntry> {
    let pitch = drum.attribute("pitch")?.trim().parse::<u8>().ok()?;
    let text_of = |tag_name: &str| {
        drum.children()
            .find(|child| child.is_element() && child.has_tag_name(tag_name))
            .and_then(|node| node.text())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };

    Some(DrumMapEntry {
        pitch,
        name: text_of("name")?,
        head: text_of("head"),
        line: text_of("line").and_then(|value| value.parse::<i8>().ok()),
        voice: text_of("voice").and_then(|value| value.parse::<u8>().ok()),
        stem: text_of("stem").and_then(|value| value.parse::<i8>().ok()),
        shortcut: text_of("shortcut"),
    })
}

fn musescore_part_name_aliases(part: Node<'_, '_>, instrument: Node<'_, '_>) -> HashSet<String> {
    let mut aliases = HashSet::new();

    for node in [part, instrument] {
        for tag_name in ["trackName", "longName", "shortName"] {
            if let Some(name) = node
                .children()
                .find(|child| child.is_element() && child.has_tag_name(tag_name))
                .and_then(|node| node.text())
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                aliases.insert(normalize_track_lookup_key(name));
            }
        }
    }

    aliases
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
    pub drum_map: Option<Vec<DrumMapEntry>>,
}

type ForcedProgramSequenceMap = HashMap<String, Vec<Option<u8>>>;
type DrumsetMappingMap = HashMap<String, Vec<DrumMapEntry>>;

struct TrackInfo {
    /// Index into the original MIDI track list / raw MTrk chunk list.
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

    let musicxml_path = output_dir.join("score.musicxml");
    let forced_program_sequences = if musicxml_path.exists() {
        tracing::info!("stems: reusing existing MusicXML file for articulation analysis");
        match tokio::fs::read_to_string(&musicxml_path).await {
            Ok(xml) => parse_musicxml_forced_program_sequences(&xml),
            Err(error) => {
                tracing::warn!("stems: could not read existing MusicXML file: {error}");
                HashMap::new()
            }
        }
    } else {
        match generate_musicxml(config, input_path, output_dir).await? {
            ConversionOutcome::Ready { bytes, .. } => {
                tracing::info!("stems: MusicXML export complete for articulation analysis");
                match String::from_utf8(bytes.to_vec()) {
                    Ok(xml) => parse_musicxml_forced_program_sequences(&xml),
                    Err(error) => {
                        tracing::warn!("stems: MusicXML export was not valid UTF-8: {error}");
                        HashMap::new()
                    }
                }
            }
            ConversionOutcome::Unavailable { reason } => {
                tracing::warn!(
                    "stems: MusicXML export unavailable for articulation analysis: {reason}"
                );
                HashMap::new()
            }
            ConversionOutcome::Failed { reason } => {
                tracing::warn!("stems: MusicXML export failed for articulation analysis: {reason}");
                HashMap::new()
            }
        }
    };

    let drumset_mappings = match extract_musescore_drumset_mappings(input_path) {
        Ok(mappings) => mappings,
        Err(error) => {
            tracing::warn!(
                "stems: failed to extract MuseScore drumset mappings from '{}': {error}",
                input_path.display()
            );
            HashMap::new()
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

    // Build one clean tempo track from all MIDI tracks. Some MuseScore exports
    // place the real tempo map outside raw track 0, so assuming chunk 0 owns
    // tempo can make rendered stems fall back to the wrong BPM.
    let clean_tempo_chunk = build_global_tempo_chunk(&midi_bytes);

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
        drum_map: Option<Vec<DrumMapEntry>>,
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

        let sfz_path = match sfz_for_gm_program(
            track_info.program,
            track_info.is_percussion,
            &sfz_dir,
            &sfz_mapping,
        ) {
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
                    Some(ProgramEntry::Detailed(d)) if !d.overrides.is_empty() => {
                        Some(&d.overrides)
                    }
                    _ => None,
                }
            } else {
                None
            };
            if let Some(overrides) = overrides {
                let forced_programs = forced_program_sequences
                    .get(&normalize_track_lookup_key(&track_info.track_name))
                    .map(Vec::as_slice);
                let groups = extract_program_groups(&midi_bytes, chunk_idx, forced_programs);
                tracing::debug!(
                    "stems: '{}' program-groups found: {:?}",
                    track_info.track_name,
                    groups.keys().collect::<Vec<_>>()
                );
                let canon_events = groups.get(&track_info.program).cloned().unwrap_or_default();
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
                    if *prog == track_info.program || events.is_empty() {
                        continue;
                    }
                    let prog_key = prog.to_string();
                    if let Some(sfz_rel) = overrides.get(&prog_key) {
                        let sfz_p = sfz_dir.join(sfz_rel);
                        if sfz_p.exists() {
                            let extra_mtrk = build_mtrk(events.clone());
                            let extra_midi =
                                build_stem_midi(&midi_bytes, &clean_tempo_chunk, &extra_mtrk);
                            let rel_path = gain_path_key_for_soundfont(&sfz_p, &sfz_dir);
                            let gain_key = format!("{}.override.{}", track_info.program, prog);
                            let gain = lookup_gain(
                                &sfz_mapping,
                                rel_path.as_deref(),
                                &[gain_key.as_str()],
                            );
                            let n = extras.len();
                            extras.push(ExtraStem {
                                sfz_path: sfz_p,
                                midi: extra_midi,
                                mid_path: output_dir.join(format!("stem_{chunk_idx}_x{n}.mid")),
                                wav_path: output_dir.join(format!("stem_{chunk_idx}_x{n}.wav")),
                                gain_db: gain,
                            });
                        } else {
                            tracing::warn!(
                                "Program override SFZ not found: {}",
                                sfz_dir.join(sfz_rel).display()
                            );
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
                let split_input = build_stem_midi(&midi_bytes, &clean_tempo_chunk, &canonical_mtrk);
                let (stac_chunk, sus_chunk, vib_chunk) = split_midi_track_3way(
                    &split_input,
                    1,
                    staccato_sfz.is_some(),
                    vibrato_sfz.is_some(),
                );
                let sus = build_stem_midi(&midi_bytes, &clean_tempo_chunk, &sus_chunk);
                let stac = staccato_sfz
                    .is_some()
                    .then(|| build_stem_midi(&midi_bytes, &clean_tempo_chunk, &stac_chunk));
                let vib = vibrato_sfz
                    .is_some()
                    .then(|| build_stem_midi(&midi_bytes, &clean_tempo_chunk, &vib_chunk));
                (sus, stac, vib)
            } else {
                (
                    build_stem_midi(&midi_bytes, &clean_tempo_chunk, &canonical_mtrk),
                    None,
                    None,
                )
            };

        // Look up the per-instrument gain correction from mapping.json.
        // The key uses forward slashes regardless of OS.
        let main_rel_path = gain_path_key_for_soundfont(&sfz_path, &sfz_dir);
        let program_key = track_info.program.to_string();
        let gain_db = if track_info.is_percussion {
            lookup_gain(&sfz_mapping, main_rel_path.as_deref(), &["percussion"])
        } else if sfz_mapping.programs.contains_key(&program_key) {
            lookup_gain(
                &sfz_mapping,
                main_rel_path.as_deref(),
                &[program_key.as_str()],
            )
        } else {
            lookup_gain(&sfz_mapping, main_rel_path.as_deref(), &["fallback"])
        };
        let drum_map = track_info
            .is_percussion
            .then(|| {
                drumset_mappings
                    .get(&normalize_track_lookup_key(&track_info.track_name))
                    .cloned()
            })
            .flatten();

        let staccato_key = format!("{}.staccato", track_info.program);
        let staccato_rel_path = staccato_sfz
            .as_ref()
            .and_then(|path| gain_path_key_for_soundfont(path, &sfz_dir));
        let staccato_gain_db = lookup_gain(
            &sfz_mapping,
            staccato_rel_path.as_deref(),
            &[staccato_key.as_str()],
        );

        let vibrato_key = format!("{}.vibrato", track_info.program);
        let vibrato_rel_path = vibrato_sfz
            .as_ref()
            .and_then(|path| gain_path_key_for_soundfont(path, &sfz_dir));
        let vibrato_gain_db = lookup_gain(
            &sfz_mapping,
            vibrato_rel_path.as_deref(),
            &[vibrato_key.as_str()],
        );

        task_list.push(StemTask {
            stem_idx,
            chunk_idx,
            track_name: track_info.track_name.clone(),
            program: track_info.program,
            is_percussion: track_info.is_percussion,
            drum_map,
            sfz_path: sfz_path.clone(),
            stem_midi,
            stem_mid_path: output_dir.join(format!("stem_{chunk_idx}.mid")),
            stem_wav_path: output_dir.join(format!("stem_{chunk_idx}.wav")),
            stem_ogg_path: output_dir.join(format!("stem_{chunk_idx}.ogg")),
            sfizz: sfizz.clone(),
            fluidsynth: if sfz_path
                .extension()
                .map_or(false, |e| e.eq_ignore_ascii_case("sf2"))
            {
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
            let is_sf2 = task
                .sfz_path
                .extension()
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
                    .arg("-r")
                    .arg("48000")
                    .arg("-F")
                    .arg(&task.stem_wav_path)
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
            let staccato_wave_ok = if let (Some(stac_sfz), Some(stac_midi)) =
                (&task.staccato_sfz_path, &task.staccato_midi)
            {
                match tokio::fs::write(&task.staccato_mid_path, stac_midi).await {
                    Err(e) => {
                        tracing::warn!(
                            "stems: [{}/{}] '{}' – staccato MIDI write: {e}",
                            task.stem_idx + 1,
                            total,
                            task.track_name
                        );
                        false
                    }
                    Ok(()) => {
                        match Command::new(&task.sfizz)
                            .arg("--sfz")
                            .arg(stac_sfz)
                            .arg("--midi")
                            .arg(&task.staccato_mid_path)
                            .arg("--wav")
                            .arg(&task.staccato_wav_path)
                            .arg("--samplerate")
                            .arg("48000")
                            .output()
                            .await
                        {
                            Ok(o) if o.status.success() => {
                                tracing::info!(
                                    "stems: [{}/{}] '{}' – staccato render OK",
                                    task.stem_idx + 1,
                                    total,
                                    task.track_name
                                );
                                true
                            }
                            Ok(o) => {
                                tracing::warn!(
                                    "stems: [{}/{}] '{}' – staccato sfizz: {}",
                                    task.stem_idx + 1,
                                    total,
                                    task.track_name,
                                    String::from_utf8_lossy(&o.stderr).trim()
                                );
                                false
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "stems: [{}/{}] '{}' – staccato sfizz spawn: {e}",
                                    task.stem_idx + 1,
                                    total,
                                    task.track_name
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
            let vibrato_wave_ok = if let (Some(vib_sfz), Some(vib_midi)) =
                (&task.vibrato_sfz_path, &task.vibrato_midi)
            {
                match tokio::fs::write(&task.vibrato_mid_path, vib_midi).await {
                    Err(e) => {
                        tracing::warn!(
                            "stems: [{}/{}] '{}' – vibrato MIDI write: {e}",
                            task.stem_idx + 1,
                            total,
                            task.track_name
                        );
                        false
                    }
                    Ok(()) => {
                        match Command::new(&task.sfizz)
                            .arg("--sfz")
                            .arg(vib_sfz)
                            .arg("--midi")
                            .arg(&task.vibrato_mid_path)
                            .arg("--wav")
                            .arg(&task.vibrato_wav_path)
                            .arg("--samplerate")
                            .arg("48000")
                            .output()
                            .await
                        {
                            Ok(o) if o.status.success() => {
                                tracing::info!(
                                    "stems: [{}/{}] '{}' – vibrato render OK",
                                    task.stem_idx + 1,
                                    total,
                                    task.track_name
                                );
                                true
                            }
                            Ok(o) => {
                                tracing::warn!(
                                    "stems: [{}/{}] '{}' – vibrato sfizz: {}",
                                    task.stem_idx + 1,
                                    total,
                                    task.track_name,
                                    String::from_utf8_lossy(&o.stderr).trim()
                                );
                                false
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "stems: [{}/{}] '{}' – vibrato sfizz spawn: {e}",
                                    task.stem_idx + 1,
                                    total,
                                    task.track_name
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
                        task.stem_idx + 1,
                        total,
                        task.track_name
                    );
                    continue;
                }
                match Command::new(&task.sfizz)
                    .arg("--sfz")
                    .arg(&extra.sfz_path)
                    .arg("--midi")
                    .arg(&extra.mid_path)
                    .arg("--wav")
                    .arg(&extra.wav_path)
                    .arg("--samplerate")
                    .arg("48000")
                    .output()
                    .await
                {
                    Ok(o) if o.status.success() => {
                        tracing::info!(
                            "stems: [{}/{}] '{}' – extra stem OK ({})",
                            task.stem_idx + 1,
                            total,
                            task.track_name,
                            extra
                                .sfz_path
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                        );
                        extra_wav_ok.push((extra.wav_path.clone(), extra.gain_db));
                    }
                    Ok(o) => tracing::warn!(
                        "stems: [{}/{}] '{}' – extra stem sfizz ({}): {}",
                        task.stem_idx + 1,
                        total,
                        task.track_name,
                        extra
                            .sfz_path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy(),
                        String::from_utf8_lossy(&o.stderr).trim()
                    ),
                    Err(e) => tracing::warn!(
                        "stems: [{}/{}] '{}' – extra stem sfizz spawn: {e}",
                        task.stem_idx + 1,
                        total,
                        task.track_name
                    ),
                }
            }

            // Encode WAV → Opus.  Collect all stems that rendered successfully
            // (sustain always present; staccato, vibrato, extra optional), apply
            // per-SFZ gain correction, and mix in a single ffmpeg pass.
            let mut sources: Vec<(&PathBuf, f64)> = vec![(&task.stem_wav_path, task.gain_db)];
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
                    ffmpeg_cmd
                        .arg("-af")
                        .arg(format!("volume={:.2}dB", sources[0].1));
                }
            } else {
                let n = sources.len();
                let mut filter = String::new();
                for (i, (_, gain)) in sources.iter().enumerate() {
                    filter.push_str(&format!("[{i}:a]volume={:.2}dB[a{i}];", gain));
                }
                let inputs: String = (0..n).map(|i| format!("[a{i}]")).collect();
                filter.push_str(&format!("{}amix=inputs={n}:normalize=0[aout]", inputs));
                ffmpeg_cmd
                    .arg("-filter_complex")
                    .arg(&filter)
                    .arg("-map")
                    .arg("[aout]");
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
            match ffmpeg_cmd.output().await {
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
                        drum_map: task.drum_map,
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
        tracing::info!(
            "stems: render complete – {}/{total} stems produced",
            stems.len()
        );
        Ok((stems, "ready".to_owned(), None))
    }
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
        if Command::new(candidate).arg("--help").output().await.is_ok() {
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
        if probe_musescore_binary(candidate).await {
            return Some(candidate.to_owned());
        }
    }

    for candidate in platform_musescore_binary_candidates() {
        if probe_musescore_binary(&candidate).await {
            return Some(candidate);
        }
    }

    None
}

#[derive(Clone, Debug)]
enum MuseScoreCommand {
    Native { binary: String },
    Docker { image: String },
}

async fn probe_musescore_binary(binary: &str) -> bool {
    Command::new(binary)
        .arg("--long-version")
        .output()
        .await
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "windows")]
fn platform_musescore_binary_candidates() -> Vec<String> {
    vec![
        r"C:\Program Files\MuseScore Studio 4\bin\MuseScoreStudio.exe".to_owned(),
        r"C:\Program Files\MuseScore 4\bin\MuseScore4.exe".to_owned(),
        r"C:\Program Files\MuseScore 3\bin\MuseScore3.exe".to_owned(),
    ]
}

#[cfg(target_os = "macos")]
fn platform_musescore_binary_candidates() -> Vec<String> {
    vec![
        "/Applications/MuseScore Studio 4.app/Contents/MacOS/mscore".to_owned(),
        "/Applications/MuseScore 4.app/Contents/MacOS/mscore".to_owned(),
        "/Applications/MuseScore 3.app/Contents/MacOS/mscore".to_owned(),
    ]
}

#[cfg(target_os = "linux")]
fn platform_musescore_binary_candidates() -> Vec<String> {
    vec![
        "/usr/local/bin/musescore4".to_owned(),
        "/usr/local/bin/musescore".to_owned(),
        "/usr/bin/musescore4".to_owned(),
        "/usr/bin/musescore".to_owned(),
        "/usr/bin/mscore".to_owned(),
        "/opt/musescore4/AppRun".to_owned(),
    ]
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn platform_musescore_binary_candidates() -> Vec<String> {
    Vec::new()
}

async fn find_musescore_command(config: &AppConfig) -> Option<MuseScoreCommand> {
    if let Some(image) = &config.musescore_docker_image {
        return Some(MuseScoreCommand::Docker {
            image: image.clone(),
        });
    }

    find_musescore_binary(config)
        .await
        .map(|binary| MuseScoreCommand::Native { binary })
}

fn native_musescore_command(binary: &str, config: &AppConfig, xdg_runtime_dir: &Path) -> Command {
    let mut command = Command::new(binary);

    #[cfg(target_os = "linux")]
    {
        command
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .env("XDG_RUNTIME_DIR", xdg_runtime_dir);

        let qt_qpa_platform = config
            .musescore_qt_platform
            .clone()
            .unwrap_or_else(|| "offscreen".to_owned());
        command.env("QT_QPA_PLATFORM", qt_qpa_platform);
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = xdg_runtime_dir;
        if let Some(qt_qpa_platform) = &config.musescore_qt_platform {
            command.env("QT_QPA_PLATFORM", qt_qpa_platform);
        } else {
            command.env_remove("QT_QPA_PLATFORM");
        }
    }

    command
}

fn docker_musescore_command(
    config: &AppConfig,
    image: &str,
    input_path: &Path,
    output_path: &Path,
) -> Result<Command> {
    let input_dir = input_path
        .parent()
        .context("MuseScore input path has no parent directory")?
        .canonicalize()
        .with_context(|| format!("failed to resolve input directory {}", input_path.display()))?;
    let output_dir = output_path
        .parent()
        .context("MuseScore output path has no parent directory")?
        .canonicalize()
        .with_context(|| {
            format!(
                "failed to resolve output directory {}",
                output_path.display()
            )
        })?;

    let input_file_name = file_name(input_path)?;
    let output_file_name = file_name(output_path)?;

    let mut command = Command::new(&config.docker_bin);
    command
        .arg("run")
        .arg("--rm")
        .arg("--mount")
        .arg(bind_mount_arg(&input_dir, "/work/input", true))
        .arg("--mount")
        .arg(bind_mount_arg(&output_dir, "/work/output", false))
        .arg(image)
        .arg("-o")
        .arg(format!("/work/output/{output_file_name}"))
        .arg(format!("/work/input/{input_file_name}"));
    Ok(command)
}

fn bind_mount_arg(source: &Path, target: &str, readonly: bool) -> String {
    let mut arg = format!("type=bind,source={},target={target}", source.display());
    if readonly {
        arg.push_str(",readonly");
    }
    arg
}

fn file_name(path: &Path) -> Result<&str> {
    path.file_name()
        .and_then(OsStr::to_str)
        .with_context(|| format!("path '{}' has no valid UTF-8 file name", path.display()))
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
    let Some(command_kind) = find_musescore_command(config).await else {
        return Ok(ConversionOutcome::Unavailable {
            reason: unavailable_reason.to_owned(),
        });
    };

    tracing::info!(
        "musescore: converting '{}' → {}",
        input_path.file_name().unwrap_or_default().to_string_lossy(),
        output_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy(),
    );

    let xdg_runtime_dir = std::env::temp_dir().join("fumen-musescore-runtime");
    tokio::fs::create_dir_all(&xdg_runtime_dir)
        .await
        .with_context(|| format!("failed to create {}", xdg_runtime_dir.display()))?;

    let (runner_label, mut command) = match &command_kind {
        MuseScoreCommand::Native { binary } => {
            let mut command = native_musescore_command(binary, config, &xdg_runtime_dir);
            command.arg("-o").arg(output_path).arg(input_path);
            (binary.as_str().to_owned(), command)
        }
        MuseScoreCommand::Docker { image } => (
            format!("{} run {}", config.docker_bin, image),
            docker_musescore_command(config, image, input_path, output_path)?,
        ),
    };

    let command_output = command
        .output()
        .await
        .with_context(|| format!("failed to start MuseScore converter '{runner_label}'"))?;

    if !command_output.status.success() {
        let stderr =
            sanitize_musescore_output(String::from_utf8_lossy(&command_output.stderr).as_ref());
        let stdout =
            sanitize_musescore_output(String::from_utf8_lossy(&command_output.stdout).as_ref());
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
                format!("MuseScore converter '{runner_label}' failed with {status}.")
            } else {
                format!("MuseScore converter '{runner_label}' failed with {status}.\n{detail}")
            },
        });
    }

    let bytes = tokio::fs::read(output_path)
        .await
        .with_context(|| format!("failed to read generated file at {}", output_path.display()))?;

    tracing::info!("musescore: done ({} KB)", bytes.len() / 1024,);

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
        mapping
            .percussion
            .as_deref()
            .unwrap_or("VSCO-2-CE-1.1.0/GM-StylePerc.sfz")
    } else {
        let key = program.to_string();
        match mapping.programs.get(&key) {
            Some(ProgramEntry::Simple(p)) => p.as_str(),
            Some(ProgramEntry::Detailed(d)) => d.sfz.as_str(),
            None => mapping
                .fallback
                .as_deref()
                .unwrap_or("VSCO-2-CE-1.1.0/UprightPiano.sfz"),
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
const VIBRATO_THRESHOLD_US: u64 = 800_000;

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

    let tempo_map = build_global_tempo_map(&smf);

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
    let mut vibrato_set: HashSet<(u8, u8, u32)> = HashSet::new();

    for (i, ev) in track.iter().enumerate() {
        let tick = abs_ticks[i];
        let TrackEventKind::Midi { channel, message } = &ev.kind else {
            continue;
        };
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
    let mut open_vib: HashSet<(u8, u8)> = HashSet::new();
    let mut stac_events: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut sus_events: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut vib_events: Vec<(u32, Vec<u8>)> = Vec::new();

    let mut abs = 0u32;
    for ev in track.iter() {
        abs = abs.saturating_add(u32::from(ev.delta));
        let tick = abs;
        let TrackEventKind::Midi { channel, message } = &ev.kind else {
            continue;
        };
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

    (
        build_mtrk(stac_events),
        build_mtrk(sus_events),
        build_mtrk(vib_events),
    )
}

/// Collect tempo change events from all MIDI tracks into one sorted
/// `Vec<(abs_tick, µs_per_quarter_note)>`. Defaults to 120 BPM when no tempo
/// event exists at tick 0.
fn build_global_tempo_map(smf: &Smf<'_>) -> Vec<(u32, u32)> {
    let mut map: Vec<(u32, u32)> = vec![(0, 500_000)];
    let mut tempo_events: Vec<(u32, u32)> = Vec::new();

    for track in &smf.tracks {
        let mut abs = 0u32;
        for ev in track {
            abs = abs.saturating_add(u32::from(ev.delta));
            if let TrackEventKind::Meta(MetaMessage::Tempo(t)) = &ev.kind {
                tempo_events.push((abs, u32::from(*t)));
            }
        }
    }

    tempo_events.sort_by_key(|(tick, _)| *tick);
    for (tick, us) in tempo_events {
        if let Some(last) = map.last_mut() {
            if last.0 == tick {
                last.1 = us;
                continue;
            }
            if last.1 == us {
                continue;
            }
        }
        map.push((tick, us));
    }

    map
}

/// Build a clean MTrk chunk that carries the global tempo map for the score.
/// Tempo events are collected from all tracks because some MuseScore exports do
/// not store them in raw track 0.
fn build_global_tempo_chunk(midi_bytes: &[u8]) -> Vec<u8> {
    let smf = match Smf::parse(midi_bytes) {
        Ok(smf) => smf,
        Err(error) => {
            tracing::warn!(
                "stems: could not parse MIDI tempo map, falling back to track 0 meta events: {error}"
            );
            return extract_raw_midi_chunks(midi_bytes)
                .first()
                .map(|chunk| strip_channel_events(chunk))
                .unwrap_or_else(|| build_mtrk(Vec::new()));
        }
    };

    let events = build_global_tempo_map(&smf)
        .into_iter()
        .filter_map(|(tick, us_per_qn)| {
            if tick == 0 && us_per_qn == 500_000 {
                return None;
            }

            let bytes = us_per_qn.to_be_bytes();
            Some((tick, vec![0xFF, 0x51, 0x03, bytes[1], bytes[2], bytes[3]]))
        })
        .collect();

    build_mtrk(events)
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
        let seg_end = tempo_map.get(i + 1).map(|t| t.0).unwrap_or(u32::MAX);
        if seg_end <= cursor {
            continue;
        }
        if seg_start >= end_tick {
            break;
        }
        let ticks = (end_tick.min(seg_end) - cursor.max(seg_start)) as u64;
        us += ticks * seg_tempo / ticks_per_qn as u64;
        cursor = end_tick.min(seg_end);
        if cursor >= end_tick {
            break;
        }
    }
    us
}

/// Encode a single MIDI channel-voice event to raw bytes (no delta prefix).
/// Returns an empty `Vec` for event types that are intentionally skipped.
fn encode_midi_event(channel: u8, message: &MidiMessage) -> Vec<u8> {
    match message {
        MidiMessage::NoteOn { key, vel } => vec![0x90 | channel, u8::from(*key), u8::from(*vel)],
        MidiMessage::NoteOff { key, vel } => vec![0x80 | channel, u8::from(*key), u8::from(*vel)],
        MidiMessage::Controller { controller, value } => {
            vec![0xB0 | channel, u8::from(*controller), u8::from(*value)]
        }
        MidiMessage::ProgramChange { program } => vec![0xC0 | channel, u8::from(*program)],
        MidiMessage::Aftertouch { key, vel } => {
            vec![0xA0 | channel, u8::from(*key), u8::from(*vel)]
        }
        MidiMessage::ChannelAftertouch { vel } => vec![0xD0 | channel, u8::from(*vel)],
        MidiMessage::PitchBend { bend } => {
            // bend.0 is the raw u14 value: 0x0000=min, 0x2000=center, 0x3FFF=max
            let raw = u16::from(bend.0);
            vec![
                0xE0 | channel,
                (raw & 0x7F) as u8,
                ((raw >> 7) & 0x7F) as u8,
            ]
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
    forced_programs: Option<&[Option<u8>]>,
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
    let mut note_index: usize = 0;

    let mut abs = 0u32;
    for ev in track.iter() {
        abs = abs.saturating_add(u32::from(ev.delta));
        let tick = abs;
        let TrackEventKind::Midi { channel, message } = &ev.kind else {
            continue;
        };
        let ch = u8::from(*channel);
        match message {
            MidiMessage::ProgramChange { program } => {
                current_program[ch as usize] = u8::from(*program);
                // Not forwarded to sfizz — sfizz uses the --sfz file directly.
            }
            MidiMessage::NoteOn { key, vel } if u8::from(*vel) > 0 => {
                let note = u8::from(*key);
                let forced_prog = forced_programs
                    .and_then(|programs| programs.get(note_index))
                    .copied()
                    .flatten();
                let prog = forced_prog.unwrap_or(current_program[ch as usize]);
                note_index += 1;
                open_notes.insert((ch, note), prog);
                let enc = encode_midi_event(ch, message);
                groups.entry(prog).or_default().push((tick, enc));
            }
            MidiMessage::NoteOff { key, .. } | MidiMessage::NoteOn { key, .. } => {
                let note = u8::from(*key);
                let prog = open_notes
                    .remove(&(ch, note))
                    .unwrap_or(current_program[ch as usize]);
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

fn parse_musicxml_forced_program_sequences(xml: &str) -> ForcedProgramSequenceMap {
    let doc = match Document::parse_with_options(
        xml,
        ParsingOptions {
            allow_dtd: true,
            ..ParsingOptions::default()
        },
    ) {
        Ok(doc) => doc,
        Err(error) => {
            tracing::warn!("stems: could not parse MusicXML for articulation analysis: {error}");
            return HashMap::new();
        }
    };

    let mut part_aliases: HashMap<String, Vec<String>> = HashMap::new();
    for score_part in doc
        .descendants()
        .filter(|node| node.has_tag_name("score-part"))
    {
        let Some(id) = score_part.attribute("id") else {
            continue;
        };

        let mut aliases = Vec::new();
        for child in score_part.children().filter(|child| child.is_element()) {
            if child.has_tag_name("part-name") || child.has_tag_name("part-abbreviation") {
                let alias = child.text().unwrap_or("").trim();
                if !alias.is_empty() {
                    aliases.push(alias.to_owned());
                }
                continue;
            }

            if child.has_tag_name("score-instrument") {
                for instrument_child in child.children().filter(|node| node.is_element()) {
                    if instrument_child.has_tag_name("instrument-name") {
                        let alias = instrument_child.text().unwrap_or("").trim();
                        if !alias.is_empty() {
                            aliases.push(alias.to_owned());
                        }
                    }
                }
            }
        }

        if !aliases.is_empty() {
            aliases.sort();
            aliases.dedup();
            part_aliases.insert(id.to_owned(), aliases);
        }
    }

    let mut result: ForcedProgramSequenceMap = HashMap::new();
    for part in doc.descendants().filter(|node| node.has_tag_name("part")) {
        let Some(part_id) = part.attribute("id") else {
            continue;
        };
        let Some(aliases) = part_aliases.get(part_id) else {
            continue;
        };

        let forced = collect_part_forced_programs(part);
        if forced.iter().any(|program| program.is_some()) {
            for alias in aliases {
                let key = normalize_track_lookup_key(alias);
                if key.is_empty() {
                    continue;
                }

                match result.get(&key) {
                    Some(existing) if existing != &forced => {
                        tracing::warn!(
                            "stems: conflicting MusicXML articulation aliases for '{}'; keeping first match",
                            alias
                        );
                    }
                    Some(_) => {}
                    None => {
                        result.insert(key, forced.clone());
                    }
                }
            }
        }
    }

    result
}

fn normalize_track_lookup_key(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '♭' => normalized.push('b'),
            '♯' => normalized.push_str("sharp"),
            '#' => normalized.push_str("sharp"),
            _ if ch.is_alphanumeric() => normalized.extend(ch.to_lowercase()),
            _ => {}
        }
    }
    normalized
}

fn collect_part_forced_programs(part: Node<'_, '_>) -> Vec<Option<u8>> {
    let mut pizzicato_active = false;
    let mut forced_programs = Vec::new();

    for measure in part
        .children()
        .filter(|child| child.is_element() && child.has_tag_name("measure"))
    {
        for child in measure.children().filter(|child| child.is_element()) {
            if child.has_tag_name("direction") {
                update_pizzicato_state_from_direction(child, &mut pizzicato_active);
                continue;
            }

            if child.has_tag_name("sound") {
                update_pizzicato_state_from_sound(child, &mut pizzicato_active);
                continue;
            }

            if child.has_tag_name("note") && is_sounded_musicxml_note(child) {
                forced_programs.push(pizzicato_active.then_some(45));
            }
        }
    }

    forced_programs
}

fn update_pizzicato_state_from_direction(direction: Node<'_, '_>, pizzicato_active: &mut bool) {
    for descendant in direction.descendants().filter(|node| node.is_element()) {
        if descendant.has_tag_name("sound") {
            update_pizzicato_state_from_sound(descendant, pizzicato_active);
            continue;
        }

        if descendant.has_tag_name("words") {
            let text = descendant.text().unwrap_or("").trim().to_ascii_lowercase();
            if text.contains("pizz") {
                *pizzicato_active = true;
            } else if text.contains("arco") {
                *pizzicato_active = false;
            }
        }
    }
}

fn update_pizzicato_state_from_sound(sound: Node<'_, '_>, pizzicato_active: &mut bool) {
    if let Some(value) = sound.attribute("pizzicato") {
        match value {
            "yes" => *pizzicato_active = true,
            "no" => *pizzicato_active = false,
            _ => {}
        }
    }
}

fn is_sounded_musicxml_note(note: Node<'_, '_>) -> bool {
    let has_pitch = note.children().any(|child| {
        child.is_element() && (child.has_tag_name("pitch") || child.has_tag_name("unpitched"))
    });
    if !has_pitch {
        return false;
    }

    !note.children().any(|child| {
        child.is_element()
            && (child.has_tag_name("rest")
                || child.has_tag_name("grace")
                || child.has_tag_name("cue"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_path(relative: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
    }

    #[test]
    fn lookup_gain_prefers_semantic_program_keys() {
        let mapping = SfzMapping {
            percussion: None,
            fallback: None,
            programs: HashMap::new(),
            gains: HashMap::from([
                ("42".to_owned(), -3.5),
                ("data/VSCO-2-CE-1.1.0/CelloEnsSusVib.sfz".to_owned(), 1.0),
            ]),
        };

        assert_eq!(
            lookup_gain(
                &mapping,
                Some("data/VSCO-2-CE-1.1.0/CelloEnsSusVib.sfz"),
                &["42"],
            ),
            -3.5
        );
    }

    #[test]
    fn lookup_gain_supports_legacy_path_keys_without_data_prefix() {
        let mapping = SfzMapping {
            percussion: None,
            fallback: None,
            programs: HashMap::new(),
            gains: HashMap::from([("VSCO-2-CE-1.1.0/CelloEnsPizz.sfz".to_owned(), -11.2)]),
        };

        assert_eq!(
            lookup_gain(&mapping, Some("data/VSCO-2-CE-1.1.0/CelloEnsPizz.sfz"), &[],),
            -11.2
        );
    }

    #[test]
    fn extracts_drumset_mappings_from_musescore_archive() {
        let mappings = extract_musescore_drumset_mappings(&fixture_path(
            "data/storage/scores/068b3354-2c69-4691-8359-7bfb90c026f5/Chrono_Trigger_-_Main_Theme.mscz",
        ))
        .expect("fixture mscz should be readable");

        let drumset = mappings
            .get(&normalize_track_lookup_key("Set de batterie"))
            .expect("drumset mapping should exist");

        assert!(
            drumset
                .iter()
                .any(|entry| entry.pitch == 38 && entry.name == "Snare")
        );
        assert!(
            drumset
                .iter()
                .any(|entry| entry.pitch == 49 && entry.name == "Crash Cymbal")
        );
    }

    #[test]
    fn normalize_track_lookup_key_handles_accidentals_and_spacing() {
        assert_eq!(
            normalize_track_lookup_key("Clarinette en Si♭ 1"),
            normalize_track_lookup_key("Clarinette en Sib 1")
        );
        assert_eq!(
            normalize_track_lookup_key("Trompette en Si♭"),
            normalize_track_lookup_key("Trompette en Sib")
        );
    }

    #[test]
    fn cello_fixture_marks_every_note_as_pizzicato() {
        let xml = std::fs::read_to_string(fixture_path(
            "data/storage/musicxml/a20e4d85-d2cb-4c17-ab5e-e47a30fdf613.musicxml",
        ))
        .expect("fixture musicxml should be readable");

        let sequences = parse_musicxml_forced_program_sequences(&xml);
        let cello = sequences
            .get(&normalize_track_lookup_key("Violoncelle"))
            .expect("cello forced sequence should exist");

        assert_eq!(cello.len(), 236);
        assert!(cello.iter().all(|program| *program == Some(45)));
    }

    #[test]
    fn cello_fixture_extracts_only_pizzicato_group_when_forced() {
        let midi = std::fs::read(fixture_path(
            "data/storage/midi/a20e4d85-d2cb-4c17-ab5e-e47a30fdf613.mid",
        ))
        .expect("fixture midi should be readable");
        let xml = std::fs::read_to_string(fixture_path(
            "data/storage/musicxml/a20e4d85-d2cb-4c17-ab5e-e47a30fdf613.musicxml",
        ))
        .expect("fixture musicxml should be readable");

        let sequences = parse_musicxml_forced_program_sequences(&xml);
        let track_info = parse_midi_tracks(&midi)
            .into_iter()
            .find(|track| track.track_name == "Violoncelle")
            .expect("cello track should exist");

        let forced = sequences
            .get(&normalize_track_lookup_key(&track_info.track_name))
            .expect("cello forced sequence should exist");
        let groups =
            extract_program_groups(&midi, track_info.midi_track_index, Some(forced.as_slice()));

        assert!(groups.contains_key(&45));
        assert!(!groups.contains_key(&42));
        assert_eq!(groups.len(), 1);
    }
}
