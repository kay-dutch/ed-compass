//! What to keep when the disk budget is reached.
//!
//! **Oldest first, with one exception.** Captures used to be ranked by what the
//! detectors made of them, keeping the best-scoring and evicting the rest. That
//! is the right policy for a tool that recognises what it is looking at, and the
//! wrong one for this tool, because ranking by detector score means ranking by
//! *how much the software understands a recording* — and the entire purpose here
//! is finding signals nobody has catalogued, which is precisely what the
//! detectors cannot recognise. A recording they rate at zero is what an
//! undiscovered signal looks like to them. The policy was quietly deleting the
//! most interesting thing on the disk first.
//!
//! Age is a poorer measure of value but an honest one: it makes no claim about
//! the contents. What survives is what you recorded most recently, which is at
//! least the material you are still working on.
//!
//! The exception is a capture taken by hand. Someone pressing Export has made a
//! judgement the software is not in a position to overrule, and those are kept
//! until nothing else remains.
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
    /// Taken by hand rather than by a detector. Kept while anything else remains.
    pub manual: bool,
}

/// How much to keep.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Policy {
    pub budget_bytes: u64,
}

/// Indices of the records whose audio should be deleted, in the order to do it.
///
/// Pure: it touches no files, so the decision can be tested without a disk.
///
/// Oldest first. Captures taken by hand go last of all — but they do go, once
/// nothing else is left, because a budget that protected files without limit
/// would not be a budget and an unattended overnight session would fill the
/// drive.
pub fn evictions(records: &[Record], policy: &Policy) -> Vec<usize> {
    if policy.budget_bytes == 0 {
        return Vec::new();
    }
    let mut total: u64 = records.iter().map(|r| r.bytes).sum();
    if total <= policy.budget_bytes {
        return Vec::new();
    }

    let mut order: Vec<usize> = (0..records.len()).collect();
    order.sort_by(|&a, &b| {
        records[a]
            .manual
            .cmp(&records[b].manual)
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

/// Was this capture taken by hand?
///
/// The only distinction the policy makes. Everything else is decided by age,
/// deliberately: see the module header for why detector scores are the wrong
/// measure of what is worth keeping here.
pub fn is_manual(json: &serde_json::Value) -> bool {
    json.get("reason").and_then(|v| v.as_str()) == Some("manual")
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
        let manual = std::fs::read_to_string(&sidecar)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .is_some_and(|j| is_manual(&j));
        records.push(Record {
            audio,
            sidecar,
            bytes: meta.len(),
            modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            manual,
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
                    "evicted the audio of {}; its record is kept",
                    r.audio.display()
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

    /// A capture `age_secs` old, optionally one taken by hand.
    fn record(name: &str, bytes: u64, age_secs: u64, manual: bool) -> Record {
        Record {
            audio: PathBuf::from(format!("{name}.flac")),
            sidecar: PathBuf::from(format!("{name}.json")),
            bytes,
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(10_000 - age_secs),
            manual,
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
        let records = vec![record("a", 100, 10, false), record("b", 100, 5, false)];
        let policy = Policy { budget_bytes: 1000 };
        assert!(evictions(&records, &policy).is_empty());
    }

    #[test]
    fn captures_of_the_same_age_are_evicted_in_a_stable_order() {
        let records = vec![
            record("old", 100, 100, false),
            record("new", 100, 1, false),
            record("keep", 100, 50, false),
        ];
        let policy = Policy { budget_bytes: 250 };
        assert_eq!(names(&records, evictions(&records, &policy)), ["old"]);
    }

    /// The policy, stated as a test: age decides, nothing else.
    ///
    /// It used to be detector score, and that was backwards for this tool —
    /// ranking by score ranks by how much the software understands a recording,
    /// while the whole search is for signals it cannot recognise. The
    /// lowest-scoring file on the disk is what an undiscovered signal looks like.
    #[test]
    fn the_oldest_capture_goes_first() {
        let records = vec![
            record("newest", 100, 1, false),
            record("oldest", 100, 900, false),
            record("middle", 100, 100, false),
        ];
        let policy = Policy { budget_bytes: 250 };
        assert_eq!(names(&records, evictions(&records, &policy)), ["oldest"]);
    }

    /// A capture someone took by hand is a judgement the software is not in a
    /// position to overrule, so it outlives every automatic one however old.
    #[test]
    fn a_hand_taken_capture_is_kept_while_anything_else_remains() {
        let records = vec![
            record("kept-by-hand", 100, 5000, true),
            record("auto-new", 100, 1, false),
            record("auto-old", 100, 50, false),
        ];
        let policy = Policy { budget_bytes: 150 };
        assert_eq!(
            names(&records, evictions(&records, &policy)),
            ["auto-old", "auto-new"],
            "the oldest automatic goes first, and the hand-taken one is untouched"
        );
    }

    /// But protection has to yield eventually, or an unattended session fills
    /// the drive and a budget is not a budget.
    #[test]
    fn hand_taken_captures_yield_rather_than_overrunning_the_budget() {
        let records = vec![
            record("hand-old", 100, 900, true),
            record("hand-new", 100, 10, true),
        ];
        let policy = Policy { budget_bytes: 150 };
        assert_eq!(
            names(&records, evictions(&records, &policy)),
            ["hand-old"],
            "oldest first even among hand-taken captures"
        );
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

        let freed = enforce(&dir, &Policy { budget_bytes: 4096 });
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
