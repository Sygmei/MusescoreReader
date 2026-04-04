//! normalize_mapping - generate a `gains.json` file from `mapping.json`.
//!
//! Usage (run from the workspace root or the backend directory):
//!
//!   cargo run --bin normalize_mapping [--auto] [path/to/mapping.json] [output/samples/dir]
//!
//! If no mapping path is given it tries `../soundfonts/mapping.json`.
//!
//! Default mode writes a zero-gain template for every semantic gain alias
//! referenced by the mapping, such as `percussion`, `fallback`, `42`,
//! `42.staccato`, and `42.override.45`.
//!
//! `--auto` measures every mapped SFZ instrument at MIDI velocity 80
//! (mezzo-forte), computes peak-based gain corrections, and writes those values
//! into the same `gains.json` template. SF2 entries remain at 0 dB.
//!
//! If an output directory is given in `--auto` mode, raw WAVs are copied there
//! and a second set of gain-corrected WAVs is written alongside them for
//! listening comparison.
//!
//! The test note is chosen from the middle of each instrument's own SFZ key
//! range so that instruments like piccolo (which can't play C4) are measured
//! correctly. Percussive instruments (xylophone, marimba, etc.) are handled by
//! measuring peak amplitude rather than RMS, so their short attack is captured
//! instead of being buried by the silent tail.
//!
//! Only SFZ files are measured automatically; SF2 entries always stay at 0 dB.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

fn main() -> anyhow::Result<()> {
    let options = parse_args()?;
    let mapping_path = options.mapping_path.clone();
    let export_dir = options.export_dir.clone();

    let sfz_dir = mapping_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    let mapping_text = std::fs::read_to_string(&mapping_path)
        .map_err(|e| anyhow::anyhow!("Cannot read {}: {e}", mapping_path.display()))?;
    let mapping: serde_json::Value = serde_json::from_str(&mapping_text)?;
    let gain_aliases = collect_gain_aliases(&mapping);

    if gain_aliases.is_empty() {
        anyhow::bail!("No soundfont paths found in {}", mapping_path.display());
    }

    let mut gains: BTreeMap<String, f64> =
        gain_aliases.keys().cloned().map(|key| (key, 0.0)).collect();

    println!(
        "mapping      : {}",
        mapping_path
            .canonicalize()
            .unwrap_or(mapping_path.clone())
            .display()
    );
    println!("sfz dir      : {}", sfz_dir.display());
    println!(
        "mode         : {}",
        if options.auto {
            "auto peak normalization"
        } else {
            "zero-gain template"
        }
    );
    if let Some(dir) = &export_dir {
        println!("export dir   : {}", dir.display());
    }
    println!("entries      : {}", gains.len());
    println!();

    if options.auto {
        let sfizz = find_sfizz().ok_or_else(|| {
            anyhow::anyhow!(
                "sfizz_render not found in PATH. \
                 Install sfizz and ensure sfizz_render is on PATH (or set SFIZZ_BIN)."
            )
        })?;
        let ffmpeg = find_binary(&["ffmpeg", "ffmpeg.exe"]);
        println!("sfizz_render : {sfizz}");
        if let Some(ffmpeg) = &ffmpeg {
            println!("ffmpeg       : {ffmpeg}");
        }
        println!();

        let adjustments = auto_measure_gains(
            &gain_aliases,
            &sfz_dir,
            export_dir.as_deref(),
            &sfizz,
            ffmpeg.as_deref(),
        )?;
        for (key, gain) in adjustments {
            gains.insert(key, gain);
        }
    } else if export_dir.is_some() {
        println!("Note: export dir is ignored unless --auto is used.");
        println!();
    }

    let gains_path = sfz_dir.join("gains.json");
    let out = serde_json::to_string_pretty(&gains)?;
    std::fs::write(&gains_path, out)?;

    println!(
        "Wrote {} gain entries to {}",
        gains.len(),
        gains_path.display()
    );

    Ok(())
}

#[derive(Debug)]
struct CliOptions {
    auto: bool,
    mapping_path: PathBuf,
    export_dir: Option<PathBuf>,
}

fn parse_args() -> anyhow::Result<CliOptions> {
    let mut auto = false;
    let mut positional: Vec<String> = Vec::new();

    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--auto" => auto = true,
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            _ if arg.starts_with('-') => {
                anyhow::bail!("Unknown flag: {arg}\n\n{}", usage_text());
            }
            _ => positional.push(arg),
        }
    }

    if positional.len() > 2 {
        anyhow::bail!("Too many positional arguments.\n\n{}", usage_text());
    }

    Ok(CliOptions {
        auto,
        mapping_path: positional
            .first()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("../soundfonts/mapping.json")),
        export_dir: positional.get(1).map(PathBuf::from),
    })
}

fn usage_text() -> &'static str {
    "Usage: cargo run --bin normalize_mapping [--auto] [path/to/mapping.json] [output/samples/dir]"
}

fn print_usage() {
    println!("{}", usage_text());
    println!();
    println!("Default mode writes a zero-gain gains.json template from mapping.json.");
    println!("--auto measures mapped SFZ files and fills in calculated gain values.");
}

fn collect_gain_aliases(mapping: &serde_json::Value) -> BTreeMap<String, String> {
    let mut aliases = BTreeMap::new();

    let add_alias = |alias: String, source: &str, aliases: &mut BTreeMap<String, String>| {
        aliases.entry(alias).or_insert_with(|| source.to_owned());
    };

    if let Some(source) = mapping.get("percussion").and_then(|v| v.as_str()) {
        add_alias("percussion".to_owned(), source, &mut aliases);
    }

    if let Some(source) = mapping.get("fallback").and_then(|v| v.as_str()) {
        add_alias("fallback".to_owned(), source, &mut aliases);
    }

    if let Some(programs) = mapping.get("programs").and_then(|v| v.as_object()) {
        for (program, value) in programs {
            if let Some(source) = value.as_str() {
                add_alias(program.clone(), source, &mut aliases);
                continue;
            }

            let Some(obj) = value.as_object() else {
                continue;
            };

            if let Some(source) = obj.get("sfz").and_then(|v| v.as_str()) {
                add_alias(program.clone(), source, &mut aliases);
            }
            if let Some(source) = obj.get("staccato").and_then(|v| v.as_str()) {
                add_alias(format!("{program}.staccato"), source, &mut aliases);
            }
            if let Some(source) = obj.get("vibrato").and_then(|v| v.as_str()) {
                add_alias(format!("{program}.vibrato"), source, &mut aliases);
            }
            if let Some(overrides) = obj.get("overrides").and_then(|v| v.as_object()) {
                for (override_program, sfz_value) in overrides {
                    if let Some(source) = sfz_value.as_str() {
                        add_alias(
                            format!("{program}.override.{override_program}"),
                            source,
                            &mut aliases,
                        );
                    }
                }
            }
        }
    }

    aliases
}

fn normalize_mapping_key(path: &str) -> String {
    path.replace('\\', "/")
}

fn auto_measure_gains(
    gain_aliases: &BTreeMap<String, String>,
    sfz_dir: &Path,
    export_dir: Option<&Path>,
    sfizz: &str,
    ffmpeg: Option<&str>,
) -> anyhow::Result<BTreeMap<String, f64>> {
    let mut measured_sources = BTreeMap::new();
    for source in gain_aliases.values() {
        if source.to_ascii_lowercase().ends_with(".sfz") {
            measured_sources
                .entry(normalize_mapping_key(source))
                .or_insert_with(|| source.clone());
        }
    }

    if measured_sources.is_empty() {
        anyhow::bail!("No SFZ entries found in mapping.json for --auto mode.");
    }

    let tmp = std::env::temp_dir().join("normalize_mapping");
    std::fs::create_dir_all(&tmp)?;
    if let Some(dir) = export_dir {
        std::fs::create_dir_all(dir)?;
    }

    println!("Working directory: {}", tmp.display());
    println!();
    println!("Measuring {} SFZ instrument(s)...", measured_sources.len());
    println!("{:-<60}", "");

    let mut level_db: HashMap<String, f64> = HashMap::new();
    let mut wav_by_source_key: HashMap<String, PathBuf> = HashMap::new();

    for (source_key, source) in &measured_sources {
        let sfz_path = sfz_dir.join(source);
        if !sfz_path.exists() {
            println!("  SKIP   {}  (file not found)", source);
            continue;
        }

        let (lo, hi) = sfz_key_range(&sfz_path).unwrap_or((60, 60));
        let note = ((lo as u16 + hi as u16) / 2).min(127) as u8;

        let safe = safe_filename(source_key);
        let midi_path = tmp.join(format!("{safe}.mid"));
        write_test_midi(&midi_path, note)?;
        let wav_path = tmp.join(format!("{safe}.wav"));

        let result = std::process::Command::new(sfizz)
            .arg("--sfz")
            .arg(&sfz_path)
            .arg("--midi")
            .arg(&midi_path)
            .arg("--wav")
            .arg(&wav_path)
            .arg("--samplerate")
            .arg("48000")
            .output();

        match result {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let msg = String::from_utf8_lossy(&o.stderr);
                let first = msg.trim().lines().next().unwrap_or("unknown error");
                println!("  FAIL   {}  - {first}", source);
                continue;
            }
            Err(e) => {
                println!("  FAIL   {}  - spawn error: {e}", source);
                continue;
            }
        }

        match measure_peak(&wav_path) {
            Ok(peak) if peak > 1e-5 => {
                let db = 20.0 * peak.log10();
                println!("  {:+7.2} dBFS  note={note:3}  {}", db, source);
                level_db.insert(source_key.clone(), db);
                wav_by_source_key.insert(source_key.clone(), wav_path.clone());

                if let Some(dir) = export_dir {
                    let dest = dir.join(format!("raw_{safe}.wav"));
                    let _ = std::fs::copy(&wav_path, &dest);
                }
            }
            Ok(_) => {
                println!(
                    "  SILENT {}  note={note:3}  (no output - wrong range? file corrupt?)",
                    source
                );
            }
            Err(e) => println!("  ERR    {}  - {e}", source),
        }
    }

    if level_db.is_empty() {
        anyhow::bail!("No instruments could be measured. Check sfizz_render and your SFZ files.");
    }

    let mut levels: Vec<f64> = level_db.values().cloned().collect();
    levels.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let target_db = levels[levels.len() / 2];

    println!();
    println!("Target level (median): {target_db:+.2} dBFS");
    println!();
    println!("Gain corrections:");
    println!("{:-<60}", "");

    let gain_by_source_key: BTreeMap<String, f64> = level_db
        .iter()
        .map(|(source_key, level)| {
            let rounded = ((target_db - level) * 10.0).round() / 10.0;
            (source_key.clone(), rounded)
        })
        .collect();

    let mut measured_gains = BTreeMap::new();
    let mut alias_adjustments: Vec<(String, f64)> = gain_aliases
        .iter()
        .filter_map(|(alias, source)| {
            let source_key = normalize_mapping_key(source);
            gain_by_source_key
                .get(&source_key)
                .copied()
                .map(|gain| (alias.clone(), gain))
        })
        .collect();
    alias_adjustments.sort_by(|a, b| a.0.cmp(&b.0));

    for (alias, gain) in &alias_adjustments {
        println!("  {:+6.1} dB  {alias}", gain);
        measured_gains.insert(alias.clone(), *gain);
    }

    if let Some(dir) = export_dir {
        if let Some(ffmpeg) = ffmpeg {
            println!();
            println!("Writing gain-corrected WAVs to {} ...", dir.display());
            for (source_key, gain) in &gain_by_source_key {
                let Some(src_wav) = wav_by_source_key.get(source_key) else {
                    continue;
                };
                let safe = safe_filename(source_key);
                let dest = dir.join(format!("corrected_{safe}.wav"));
                let mut cmd = std::process::Command::new(ffmpeg);
                cmd.arg("-y").arg("-i").arg(src_wav);
                if gain.abs() > 0.05 {
                    cmd.arg("-af").arg(format!("volume={:.2}dB", gain));
                }
                cmd.arg("-c:a").arg("pcm_s16le").arg(&dest);
                match cmd.output() {
                    Ok(o) if o.status.success() => {
                        println!("  OK  corrected_{safe}.wav  ({:+.1} dB)", gain);
                    }
                    Ok(o) => {
                        let msg = String::from_utf8_lossy(&o.stderr);
                        let first = msg.trim().lines().next().unwrap_or("?");
                        println!("  ERR corrected_{safe}.wav  - {first}");
                    }
                    Err(e) => println!("  ERR corrected_{safe}.wav  - {e}"),
                }
            }
        } else {
            println!();
            println!(
                "Note: ffmpeg not found - skipping gain-corrected export. Raw WAVs are in {}",
                dir.display()
            );
        }
    }

    Ok(measured_gains)
}

fn safe_filename(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Test MIDI generation
// ---------------------------------------------------------------------------

/// Write a minimal Format-1 MIDI file: the given `note` at velocity 80 (mf)
/// for 3 seconds, 120 BPM, 480 ticks per quarter-note, GM program 0.
///
/// 3 s at 120 BPM = 6 quarter-notes = 2880 ticks.
/// VLQ(2880) = [0x96, 0x40]
fn write_test_midi(path: &Path, note: u8) -> anyhow::Result<()> {
    #[rustfmt::skip]
    let bytes: Vec<u8> = vec![
        0x4D, 0x54, 0x68, 0x64,
        0x00, 0x00, 0x00, 0x06,
        0x00, 0x01,
        0x00, 0x02,
        0x01, 0xE0,

        0x4D, 0x54, 0x72, 0x6B,
        0x00, 0x00, 0x00, 0x0B,
        0x00, 0xFF, 0x51, 0x03, 0x07, 0xA1, 0x20,
        0x00, 0xFF, 0x2F, 0x00,

        0x4D, 0x54, 0x72, 0x6B,
        0x00, 0x00, 0x00, 0x10,
        0x00, 0xC0, 0x00,
        0x00, 0x90, note, 0x50,
        0x96, 0x40, 0x80, note, 0x00,
        0x00, 0xFF, 0x2F, 0x00,
    ];

    std::fs::write(path, &bytes)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Peak measurement
// ---------------------------------------------------------------------------

/// Parse a WAV file and return the peak amplitude (max |sample|) over the
/// entire signal. This correctly handles percussive instruments whose energy
/// is concentrated in a brief attack. Supports 16-bit PCM, 24-bit PCM, and
/// 32-bit IEEE-float WAVs.
fn measure_peak(wav_path: &Path) -> anyhow::Result<f64> {
    let data = std::fs::read(wav_path)?;

    if data.len() < 44 {
        anyhow::bail!("file too small to be a valid WAV");
    }
    if &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        anyhow::bail!("not a RIFF/WAVE file");
    }

    let mut pos = 12usize;
    let mut audio_format: u16 = 0;
    let mut bits_per_sample: u16 = 0;
    let mut data_offset: usize = 0;
    let mut data_size: usize = 0;

    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let size = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let cstart = pos + 8;

        if id == b"fmt " && size >= 16 {
            audio_format = u16::from_le_bytes(data[cstart..cstart + 2].try_into().unwrap());
            bits_per_sample =
                u16::from_le_bytes(data[cstart + 14..cstart + 16].try_into().unwrap());
        } else if id == b"data" {
            data_offset = cstart;
            data_size = size;
            break;
        }

        pos = cstart + size + (size & 1);
    }

    if data_offset == 0 {
        anyhow::bail!("no 'data' chunk found in WAV");
    }

    let raw = &data[data_offset..(data_offset + data_size).min(data.len())];

    let samples: Vec<f64> = match (audio_format, bits_per_sample) {
        (1, 16) => raw
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as f64 / 32_768.0)
            .collect(),
        (1, 24) => raw
            .chunks_exact(3)
            .map(|b| {
                let raw_i = (b[0] as i32) | ((b[1] as i32) << 8) | ((b[2] as i32) << 16);
                let v = if raw_i & 0x80_0000 != 0 {
                    raw_i | !0xFF_FFFF
                } else {
                    raw_i
                };
                v as f64 / 8_388_608.0
            })
            .collect(),
        (3, 32) => raw
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64)
            .collect(),
        _ => anyhow::bail!("unsupported WAV format {audio_format} / {bits_per_sample}-bit"),
    };

    if samples.is_empty() {
        return Ok(0.0);
    }

    let peak = samples.iter().map(|&s| s.abs()).fold(0.0f64, f64::max);
    Ok(peak)
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

/// Locate sfizz_render: honor SFIZZ_BIN env var first, then probe PATH.
fn find_sfizz() -> Option<String> {
    if let Ok(path) = std::env::var("SFIZZ_BIN") {
        let p = path.trim().to_owned();
        if !p.is_empty() {
            return Some(p);
        }
    }
    for candidate in ["sfizz_render", "sfizz_render.exe"] {
        if std::process::Command::new(candidate)
            .arg("--help")
            .output()
            .is_ok()
        {
            return Some(candidate.to_owned());
        }
    }
    None
}

fn find_binary(candidates: &[&str]) -> Option<String> {
    for &name in candidates {
        if std::process::Command::new(name)
            .arg("--help")
            .output()
            .map(|o| o.status.success() || !o.stdout.is_empty() || !o.stderr.is_empty())
            .unwrap_or(false)
        {
            return Some(name.to_owned());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// SFZ key-range detection
// ---------------------------------------------------------------------------

/// Scan an SFZ file for `lokey`, `hikey`, and `key` opcodes and return the
/// overall `(lo, hi)` MIDI note range. Returns `None` if no key opcodes are
/// found (caller falls back to C4 = 60).
fn sfz_key_range(sfz_path: &Path) -> Option<(u8, u8)> {
    let text = std::fs::read_to_string(sfz_path).ok()?;
    let mut lo: u8 = 127;
    let mut hi: u8 = 0;
    let mut found = false;

    for line in text.lines() {
        let line = match line.find("//") {
            Some(i) => &line[..i],
            None => line,
        };
        for token in line.split_whitespace() {
            let Some((k, v)) = token.split_once('=') else {
                continue;
            };
            let midi = match note_to_midi(v.trim()) {
                Some(n) => n,
                None => continue,
            };
            match k.trim().to_ascii_lowercase().as_str() {
                "lokey" => {
                    lo = lo.min(midi);
                    found = true;
                }
                "hikey" => {
                    hi = hi.max(midi);
                    found = true;
                }
                "key" => {
                    lo = lo.min(midi);
                    hi = hi.max(midi);
                    found = true;
                }
                _ => {}
            }
        }
    }

    if found {
        let (a, b) = (lo.min(hi), lo.max(hi));
        Some((a, b))
    } else {
        None
    }
}

/// Convert an SFZ note value to a MIDI note number.
fn note_to_midi(s: &str) -> Option<u8> {
    if let Ok(n) = s.parse::<u8>() {
        return Some(n);
    }
    let s = s.to_ascii_lowercase();
    let mut chars = s.chars().peekable();
    let base: i32 = match chars.next()? {
        'c' => 0,
        'd' => 2,
        'e' => 4,
        'f' => 5,
        'g' => 7,
        'a' => 9,
        'b' => 11,
        _ => return None,
    };
    let accidental: i32 = match chars.peek() {
        Some('#') => {
            chars.next();
            1
        }
        Some('b') => {
            chars.next();
            -1
        }
        _ => 0,
    };
    let octave_str: String = chars.collect();
    let octave: i32 = octave_str.parse().ok()?;
    let midi = (octave + 1) * 12 + base + accidental;
    if (0..=127).contains(&midi) {
        Some(midi as u8)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{collect_gain_aliases, normalize_mapping_key};

    #[test]
    fn collects_semantic_gain_aliases() {
        let mapping = serde_json::json!({
            "fallback": "Soundfont.sf2",
            "percussion": "Drums.sfz",
            "programs": {
                "40": {
                    "sfz": "Strings/Main.sfz",
                    "staccato": "Strings/Stac.sfz",
                    "vibrato": "Strings/Vib.sfz",
                    "overrides": {
                        "45": "Strings/Pizz.sfz"
                    }
                },
                "41": "Strings/Main.sfz"
            }
        });

        let aliases = collect_gain_aliases(&mapping);
        let keys: Vec<_> = aliases.keys().cloned().collect();

        assert_eq!(
            keys,
            vec![
                "40",
                "40.override.45",
                "40.staccato",
                "40.vibrato",
                "41",
                "fallback",
                "percussion"
            ]
        );
    }

    #[test]
    fn normalizes_backslashes_in_gain_keys() {
        assert_eq!(
            normalize_mapping_key(r"data\VSCO-2-CE-1.1.0\CelloEnsPizz.sfz"),
            "data/VSCO-2-CE-1.1.0/CelloEnsPizz.sfz"
        );
    }
}
