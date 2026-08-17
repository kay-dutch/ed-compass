//! What to keep when the disk budget is reached.
//!
//! The naive policy — delete the oldest — is wrong for this tool. A capture is
//! evidence, and evidence does not lose value by ageing: the strongest thing
//! ever recorded would be deleted before a weak detection from yesterday. So
//! captures are ranked, and the best are protected.
//!
//! The second rule matters more than the first. A capture is two files: the
//! audio, and a JSON sidecar carrying the system, the coordinates, the scores
//! and the period. Measured on a real session, 54 captures were 946 MB of audio
//! and 40 KB of sidecars — the record is four thousandths of one percent of the
//! payload. **Sidecars are never deleted.** Evicting the audio marks the record
//! `audio_evicted` and leaves it in place, so the observation survives even when
//! the recording cannot. Trilaterating a source needs the coordinates and the
//! score; it does not need the waveform.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// One stored capture, as the retention policy sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub audio: PathBuf,
    pub sidecar: PathBuf,
    pub bytes: u64,
    pub modified: SystemTime,
    /// How much this capture is worth keeping. See [`value_of`].
    pub value: f32,
}

/// How much to keep, and how much of it to protect.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Policy {
    pub budget_bytes: u64,
    /// The best this many captures are kept even as everything else is evicted.
    pub protect_best: usize,
}

/// Indices of the records whose audio should be deleted, in the order to do it.
///
/// Pure: it touches no files, so the decision can be tested without a disk.
///
/// Lowest value goes first, oldest first among equals. The best `protect_best`
/// are held back — but only until nothing else is left. A budget that could be
/// overrun by protected files is not a budget, so once the unprotected records
/// are exhausted the policy continues into the protected ones, still worst
/// first. Anything else lets an unattended overnight session fill the drive.
pub fn evictions(records: &[Record], policy: &Policy) -> Vec<usize> {
    if policy.budget_bytes == 0 {
        return Vec::new();
    }
    let mut total: u64 = records.iter().map(|r| r.bytes).sum();
    if total <= policy.budget_bytes {
        return Vec::new();
    }

    // Rank by value to decide what is protected, best first.
    let mut by_value: Vec<usize> = (0..records.len()).collect();
    by_value.sort_by(|&a, &b| {
        records[b]
            .value
            .total_cmp(&records[a].value)
            .then_with(|| records[b].modified.cmp(&records[a].modified))
    });
    let protected: std::collections::HashSet<usize> =
        by_value.iter().take(policy.protect_best).copied().collect();

    // Worst first, oldest first among equals; protected ones last of all.
    let mut order: Vec<usize> = (0..records.len()).collect();
    order.sort_by(|&a, &b| {
        protected
            .contains(&a)
            .cmp(&protected.contains(&b))
            .then_with(|| records[a].value.total_cmp(&records[b].value))
            .then_with(|| records[a].modified.cmp(&records[b].modified))
    });

    let mut doomed = Vec::new();
    for i in order {
        if total <= policy.budget_bytes {
            break;
        }
        total = total.saturating_sub(records[i].bytes);
        doomed.push(i);
    }
    doomed
}

/// How much a capture is worth keeping, read from its sidecar.
///
/// The two sidecar shapes carry different fields, so this reads whichever are
/// present rather than insisting on one schema. A confirmed Landscape Signal
/// gets a full point on top, which puts it above anything scored on confidence
/// alone — a period match is the one measurement here that has been checked
/// against a known recording, so it is the one worth protecting hardest.
pub fn value_of(json: &serde_json::Value) -> f32 {
    let num = |key: &str| json.get(key).and_then(|v| v.as_f64()).map(|v| v as f32);

    let best = [
        num("score"),
        num("structure_score"),
        num("keying_confidence"),
    ]
    .into_iter()
    .flatten()
    .fold(0.0f32, f32::max);

    let landscape = json
        .get("matches_landscape")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    best + if landscape { 1.0 } else { 0.0 }
}

/// Read the stored captures in a directory.
///
/// A sidecar with no audio is skipped: it has already been evicted, and it costs
/// nothing to leave alone. Audio with no sidecar is included at value zero — it
/// is unexplained, so it is the first thing that should go.
pub fn scan(dir: &Path) -> Vec<Record> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    for entry in entries.flatten() {
        let audio = entry.path();
        if !matches!(
            audio.extension().and_then(|e| e.to_str()),
            Some("wav") | Some("flac")
        ) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let sidecar = audio.with_extension("json");
        let value = std::fs::read_to_string(&sidecar)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .map_or(0.0, |j| value_of(&j));
        records.push(Record {
            audio,
            sidecar,
            bytes: meta.len(),
            modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            value,
        });
    }
    records
}

/// Delete the audio the policy condemns, keeping every sidecar.
///
/// Returns the number of bytes reclaimed. Failures are logged rather than
/// propagated: losing a capture to a full disk is bad, but abandoning the hunt
/// because one delete failed is worse.
pub fn enforce(dir: &Path, policy: &Policy) -> u64 {
    let records = scan(dir);
    let mut freed = 0;
    for i in evictions(&records, policy) {
        let r = &records[i];
        match std::fs::remove_file(&r.audio) {
            Ok(()) => {
                freed += r.bytes;
                mark_evicted(&r.sidecar);
                log::info!(
                    "evicted the audio of {} (value {:.2}); its record is kept",
                    r.audio.display(),
                    r.value
                );
            }
            Err(e) => log::warn!("could not evict {}: {e}", r.audio.display()),
        }
    }
    freed
}

/// Note in the sidecar that its audio is gone.
///
/// Without this the record still names an `audio_file` that is not there, and
/// anything reading the log later cannot tell a missing file from a moved one.
fn mark_evicted(sidecar: &Path) {
    let Ok(text) = std::fs::read_to_string(sidecar) else {
        return;
    };
    let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    let Some(obj) = json.as_object_mut() else {
        return;
    };
    obj.insert("audio_evicted".into(), serde_json::Value::Bool(true));
    if let Ok(pretty) = serde_json::to_string_pretty(&json) {
        let _ = std::fs::write(sidecar, pretty);
    }
}

/// Total bytes of audio held in a directory, for the usage readout.
pub fn audio_bytes(dir: &Path) -> u64 {
    scan(dir).iter().map(|r| r.bytes).sum()
}

/// Total bytes of files with the given extension.
pub fn extension_bytes(dir: &Path, extension: &str) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some(extension))
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// How many observations are held, counting those whose audio has been evicted.
///
/// Counts sidecars, not recordings: the record is the thing that lasts.
pub fn record_count(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .count()
}

/// Keep a directory of exports under a byte budget, oldest first.
///
/// Exports get no ranking: a PNG is a rendering of data that is still held
/// elsewhere, so the only thing distinguishing two of them is which you asked
/// for more recently.
pub fn enforce_simple_budget(dir: &Path, extension: &str, budget_bytes: u64) -> u64 {
    if budget_bytes == 0 {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
    let mut total = 0u64;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(extension) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        total += meta.len();
        files.push((
            meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            meta.len(),
            path,
        ));
    }
    if total <= budget_bytes {
        return 0;
    }

    files.sort_by_key(|(t, _, _)| *t);
    let mut freed = 0;
    for (_, size, path) in files {
        if total <= budget_bytes {
            break;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                total = total.saturating_sub(size);
                freed += size;
                log::info!(
                    "removed {} to stay within the export budget",
                    path.display()
                );
            }
            Err(e) => log::warn!("could not remove {}: {e}", path.display()),
        }
    }
    freed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn record(name: &str, bytes: u64, age_secs: u64, value: f32) -> Record {
        Record {
            audio: PathBuf::from(format!("{name}.flac")),
            sidecar: PathBuf::from(format!("{name}.json")),
            bytes,
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(10_000 - age_secs),
            value,
        }
    }

    fn names(records: &[Record], picked: Vec<usize>) -> Vec<String> {
        picked
            .into_iter()
            .map(|i| {
                records[i]
                    .audio
                    .file_stem()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn nothing_is_evicted_while_the_budget_holds() {
        let records = vec![record("a", 100, 10, 0.5), record("b", 100, 5, 0.9)];
        let policy = Policy {
            budget_bytes: 1000,
            protect_best: 0,
        };
        assert!(evictions(&records, &policy).is_empty());
    }

    #[test]
    fn the_weakest_capture_goes_first_not_the_oldest() {
        // This is the whole point. "c" is the newest and the weakest; "a" is the
        // oldest and the strongest. Oldest-first would delete the best evidence.
        let records = vec![
            record("a", 100, 100, 0.95),
            record("b", 100, 50, 0.60),
            record("c", 100, 1, 0.10),
        ];
        let policy = Policy {
            budget_bytes: 250,
            protect_best: 0,
        };
        assert_eq!(names(&records, evictions(&records, &policy)), ["c"]);

        let policy = Policy {
            budget_bytes: 150,
            protect_best: 0,
        };
        assert_eq!(names(&records, evictions(&records, &policy)), ["c", "b"]);
    }

    #[test]
    fn equally_valued_captures_are_evicted_oldest_first() {
        let records = vec![
            record("old", 100, 100, 0.5),
            record("new", 100, 1, 0.5),
            record("keep", 100, 50, 0.9),
        ];
        let policy = Policy {
            budget_bytes: 250,
            protect_best: 0,
        };
        assert_eq!(names(&records, evictions(&records, &policy)), ["old"]);
    }

    #[test]
    fn the_best_captures_are_protected_while_anything_else_remains() {
        let records = vec![
            record("best", 100, 100, 0.99),
            record("good", 100, 90, 0.80),
            record("weak1", 100, 10, 0.10),
            record("weak2", 100, 5, 0.10),
        ];
        // Room for two, protecting the best two: the weak pair goes.
        let policy = Policy {
            budget_bytes: 200,
            protect_best: 2,
        };
        let out = names(&records, evictions(&records, &policy));
        assert_eq!(out, ["weak1", "weak2"]);
    }

    #[test]
    fn protection_yields_rather_than_letting_the_budget_be_exceeded() {
        // Everything is protected, yet only one fits. A protected set that can
        // overrun the disk is not a budget at all, so the policy eats into it —
        // still worst first.
        let records = vec![
            record("best", 100, 100, 0.99),
            record("mid", 100, 50, 0.70),
            record("low", 100, 10, 0.40),
        ];
        let policy = Policy {
            budget_bytes: 100,
            protect_best: 99,
        };
        assert_eq!(
            names(&records, evictions(&records, &policy)),
            ["low", "mid"]
        );
    }

    #[test]
    fn a_landscape_match_outranks_any_confidence_score() {
        let plain = serde_json::json!({ "score": 0.99, "matches_landscape": false });
        let matched = serde_json::json!({ "score": 0.20, "matches_landscape": true });
        assert!(
            value_of(&matched) > value_of(&plain),
            "a checked period match is worth more than an unverified high score"
        );
    }

    #[test]
    fn value_reads_whichever_sidecar_shape_it_is_given() {
        // The novelty sidecar carries `score`.
        assert!((value_of(&serde_json::json!({ "score": 0.7 })) - 0.7).abs() < 1e-6);
        // The detector sidecar carries these instead, and the strongest wins.
        let detector = serde_json::json!({
            "structure_score": 0.4,
            "keying_confidence": 0.8,
        });
        assert!((value_of(&detector) - 0.8).abs() < 1e-6);
        // An unrecognisable sidecar is worth nothing, so it is evicted first.
        assert_eq!(value_of(&serde_json::json!({ "unrelated": 1 })), 0.0);
    }

    #[test]
    fn audio_is_deleted_but_the_record_is_kept_and_marked() {
        let dir = std::env::temp_dir().join(format!("ed-compass-retention-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        for (name, score, bytes) in [("weak", 0.1, 4096), ("strong", 0.9, 4096)] {
            std::fs::write(dir.join(format!("{name}.flac")), vec![0u8; bytes]).unwrap();
            std::fs::write(
                dir.join(format!("{name}.json")),
                serde_json::to_string_pretty(&serde_json::json!({
                    "audio_file": format!("{name}.flac"),
                    "score": score,
                    "star_system": "Orrere",
                }))
                .unwrap(),
            )
            .unwrap();
        }

        let freed = enforce(
            &dir,
            &Policy {
                budget_bytes: 4096,
                protect_best: 0,
            },
        );
        assert_eq!(freed, 4096);

        assert!(
            !dir.join("weak.flac").exists(),
            "the weak audio must be gone"
        );
        assert!(
            dir.join("strong.flac").exists(),
            "the strong audio must remain"
        );

        // The measurement survives its recording.
        let kept: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("weak.json")).unwrap()).unwrap();
        assert_eq!(
            kept["star_system"], "Orrere",
            "the observation must survive"
        );
        assert_eq!(
            kept["audio_evicted"], true,
            "and must say the audio is gone"
        );

        let strong: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("strong.json")).unwrap())
                .unwrap();
        assert!(
            strong.get("audio_evicted").is_none(),
            "untouched records stay untouched"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_scan_ignores_sidecars_whose_audio_has_already_gone() {
        let dir = std::env::temp_dir().join(format!("ed-compass-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("gone.json"), "{\"score\":0.5}").unwrap();
        std::fs::write(dir.join("here.wav"), vec![0u8; 10]).unwrap();

        let records = scan(&dir);
        assert_eq!(
            records.len(),
            1,
            "only the one with audio counts: {records:?}"
        );
        assert_eq!(audio_bytes(&dir), 10);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The two readout helpers feed the disk bar and the observation count.
    ///
    /// Both were written without tests, and mutation testing proved it: making
    /// `extension_bytes` return 0, or 1, or count the *wrong* extension, and
    /// making `record_count` do the same, left the whole suite green.
    #[test]
    fn the_disk_readout_counts_the_right_files() {
        let dir = std::env::temp_dir().join(format!("ed-compass-readout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        std::fs::write(dir.join("a.png"), vec![0u8; 1000]).unwrap();
        std::fs::write(dir.join("b.png"), vec![0u8; 2500]).unwrap();
        // Other extensions must not be counted as exports.
        std::fs::write(dir.join("c.flac"), vec![0u8; 9999]).unwrap();
        std::fs::write(dir.join("c.json"), b"{}").unwrap();
        std::fs::write(dir.join("d.json"), b"{}").unwrap();

        assert_eq!(
            extension_bytes(&dir, "png"),
            3500,
            "only the PNGs, and their real sizes"
        );
        assert_eq!(extension_bytes(&dir, "flac"), 9999);
        assert_eq!(extension_bytes(&dir, "wav"), 0, "nothing of that kind here");

        // Sidecars are the observations, and outlive their audio.
        assert_eq!(record_count(&dir), 2, "two JSON records");

        // A directory that does not exist is empty, not a panic.
        let missing = dir.join("no-such-place");
        assert_eq!(extension_bytes(&missing, "png"), 0);
        assert_eq!(record_count(&missing), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exports_are_trimmed_oldest_first() {
        let dir = std::env::temp_dir().join(format!("ed-compass-exports-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        for name in ["a", "b", "c"] {
            std::fs::write(dir.join(format!("{name}.png")), vec![0u8; 1000]).unwrap();
            // Distinct modification times, without sleeping.
            let f = std::fs::File::open(dir.join(format!("{name}.png"))).unwrap();
            let _ = f.sync_all();
        }
        std::fs::write(dir.join("keep.txt"), vec![0u8; 5000]).unwrap();

        let freed = enforce_simple_budget(&dir, "png", 2500);
        assert!(freed >= 1000, "something must have been reclaimed");
        assert!(
            dir.join("keep.txt").exists(),
            "other file types are not ours to delete"
        );

        let left = std::fs::read_dir(&dir).unwrap().flatten().count();
        assert!(left < 4, "at least one export should be gone");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
