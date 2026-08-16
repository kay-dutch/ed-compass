//! Elite Dangerous Journal tailing.
//!
//! Read-only, and never a hard dependency: the game not running, the directory
//! not existing, or a log rotating mid-session are all normal conditions that
//! must not disturb capture. What this buys is the ability to stamp every
//! detection with a star system and its galactic coordinates — which is what
//! turns an audio scope into something that can triangulate across sessions.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// What the journal tells us about where the commander is.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GameState {
    pub star_system: Option<String>,
    /// Galactic coordinates, in light years.
    pub star_pos: Option<[f64; 3]>,
    pub system_address: Option<u64>,
    pub body: Option<String>,
    /// Current music track. Worth recording: a detection that coincides with a
    /// track change is a prime false-positive suspect.
    pub music_track: Option<String>,
    pub in_supercruise: bool,
    /// False once a `Shutdown` event is seen — audio after that is not the game.
    pub game_running: bool,
    /// Timestamp of the most recent event applied, as written by the game.
    pub last_event_utc: Option<String>,
}

impl GameState {
    /// Whether the game is playing a music track right now.
    ///
    /// Elite writes `"NoTrack"` when music is off or silent. This matters
    /// because a melodic loop is, structurally, exactly what the keying detector
    /// is looking for: a small alphabet of tones alternating on a regular clock.
    pub fn music_playing(&self) -> bool {
        matches!(self.music_track.as_deref(), Some(track) if track != "NoTrack")
    }

    /// One line for the UI header.
    pub fn describe(&self) -> String {
        match (&self.star_system, &self.star_pos) {
            (Some(system), Some([x, y, z])) => {
                format!("{system}  [{x:.1}, {y:.1}, {z:.1}]")
            }
            (Some(system), None) => system.clone(),
            _ => "unknown system".into(),
        }
    }
}

/// Tails the newest journal file in a directory.
pub struct JournalWatcher {
    dir: PathBuf,
    current: Option<PathBuf>,
    offset: u64,
    state: GameState,
    /// Set once we have failed to read the directory, so the log is not spammed
    /// every poll while the game is closed.
    warned_missing: bool,
}

impl JournalWatcher {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            current: None,
            offset: 0,
            state: GameState::default(),
            warned_missing: false,
        }
    }

    /// `%USERPROFILE%\Saved Games\Frontier Developments\Elite Dangerous`.
    ///
    /// `ED_JOURNAL_DIR` overrides it, which is also how this is tested off
    /// Windows.
    pub fn default_dir() -> Option<PathBuf> {
        if let Ok(dir) = std::env::var("ED_JOURNAL_DIR") {
            return Some(PathBuf::from(dir));
        }
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .ok()?;
        Some(
            PathBuf::from(home)
                .join("Saved Games")
                .join("Frontier Developments")
                .join("Elite Dangerous"),
        )
    }

    pub fn state(&self) -> &GameState {
        &self.state
    }

    pub fn current_file(&self) -> Option<&Path> {
        self.current.as_deref()
    }

    /// Newest `Journal*.log` by modification time.
    fn newest_journal(&self) -> Result<Option<PathBuf>> {
        let entries = std::fs::read_dir(&self.dir)
            .with_context(|| format!("reading journal directory {}", self.dir.display()))?;
        let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.starts_with("Journal") || !name.ends_with(".log") {
                continue;
            }
            let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            if best.as_ref().is_none_or(|(t, _)| modified > *t) {
                best = Some((modified, path));
            }
        }
        Ok(best.map(|(_, p)| p))
    }

    /// Read whatever has been appended since the last poll.
    ///
    /// Returns the number of events applied. Errors are swallowed into `Ok(0)`
    /// with a one-time warning — journal context is an enhancement, and losing
    /// it must never stop capture.
    pub fn poll(&mut self) -> usize {
        match self.try_poll() {
            Ok(n) => {
                self.warned_missing = false;
                n
            }
            Err(e) => {
                if !self.warned_missing {
                    log::warn!("journal unavailable ({e:#}); continuing without game context");
                    self.warned_missing = true;
                }
                0
            }
        }
    }

    fn try_poll(&mut self) -> Result<usize> {
        let newest = self.newest_journal()?;
        let Some(newest) = newest else { return Ok(0) };

        if self.current.as_ref() != Some(&newest) {
            log::info!("following journal {}", newest.display());
            self.current = Some(newest.clone());
            self.offset = 0;
        }

        let text = std::fs::read_to_string(&newest)
            .with_context(|| format!("reading {}", newest.display()))?;

        // The game rewriting or truncating the file resets us to the start
        // rather than reading from a stale offset into the middle of a line.
        let len = text.len() as u64;
        if len < self.offset {
            log::info!("journal shrank; re-reading from the beginning");
            self.offset = 0;
        }

        let fresh = &text[self.offset as usize..];
        // Only consume up to the last complete line; the game may be mid-write.
        let consumed = match fresh.rfind('\n') {
            Some(i) => i + 1,
            None => return Ok(0),
        };

        let mut applied = 0;
        for line in fresh[..consumed].lines() {
            if self.apply_line(line) {
                applied += 1;
            }
        }
        self.offset += consumed as u64;
        Ok(applied)
    }

    /// Apply one journal line. Returns whether it changed anything.
    ///
    /// Unknown events and malformed lines are ignored: the journal gains new
    /// event types with every game update, and a parse failure must never be
    /// fatal.
    pub fn apply_line(&mut self, line: &str) -> bool {
        let line = line.trim();
        if line.is_empty() {
            return false;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            return false;
        };
        let Some(event) = value.get("event").and_then(|v| v.as_str()) else {
            return false;
        };

        let timestamp = value
            .get("timestamp")
            .and_then(|v| v.as_str())
            .map(str::to_owned);

        let mut changed = true;
        match event {
            "Fileheader" => {
                self.state.game_running = true;
            }
            "FSDJump" | "CarrierJump" | "Location" => {
                if let Some(s) = value.get("StarSystem").and_then(|v| v.as_str()) {
                    self.state.star_system = Some(s.to_owned());
                }
                if let Some(pos) = value.get("StarPos").and_then(|v| v.as_array())
                    && pos.len() == 3
                {
                    let coords: Vec<f64> = pos.iter().filter_map(|v| v.as_f64()).collect();
                    if coords.len() == 3 {
                        self.state.star_pos = Some([coords[0], coords[1], coords[2]]);
                    }
                }
                self.state.system_address = value.get("SystemAddress").and_then(|v| v.as_u64());
                self.state.body = value
                    .get("Body")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
                self.state.game_running = true;
            }
            "SupercruiseEntry" => self.state.in_supercruise = true,
            "SupercruiseExit" => {
                self.state.in_supercruise = false;
                if let Some(b) = value.get("Body").and_then(|v| v.as_str()) {
                    self.state.body = Some(b.to_owned());
                }
            }
            "Music" => {
                self.state.music_track = value
                    .get("MusicTrack")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
            }
            "Shutdown" => {
                self.state.game_running = false;
                self.state.in_supercruise = false;
            }
            _ => changed = false,
        }

        if changed {
            self.state.last_event_utc = timestamp;
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watcher() -> JournalWatcher {
        JournalWatcher::new("/nonexistent")
    }

    #[test]
    fn an_fsd_jump_sets_system_and_coordinates() {
        let mut w = watcher();
        assert!(w.apply_line(
            r#"{"timestamp":"3311-08-13T14:22:07Z","event":"FSDJump","StarSystem":"Stuemeae JM-W c1-5825","SystemAddress":12345,"StarPos":[0.0,0.0,25899.0]}"#
        ));
        let s = w.state();
        assert_eq!(s.star_system.as_deref(), Some("Stuemeae JM-W c1-5825"));
        assert_eq!(s.star_pos, Some([0.0, 0.0, 25899.0]));
        assert_eq!(s.system_address, Some(12345));
        assert_eq!(s.last_event_utc.as_deref(), Some("3311-08-13T14:22:07Z"));
        assert!(s.game_running);
    }

    #[test]
    fn describe_formats_system_and_position() {
        let mut w = watcher();
        w.apply_line(r#"{"event":"Location","StarSystem":"Sol","StarPos":[0.0,0.0,0.0]}"#);
        assert_eq!(w.state().describe(), "Sol  [0.0, 0.0, 0.0]");
        assert_eq!(GameState::default().describe(), "unknown system");
    }

    #[test]
    fn supercruise_transitions_are_tracked() {
        let mut w = watcher();
        assert!(w.apply_line(r#"{"event":"SupercruiseEntry","StarSystem":"Sol"}"#));
        assert!(w.state().in_supercruise);
        assert!(w.apply_line(r#"{"event":"SupercruiseExit","Body":"Sol 4"}"#));
        assert!(!w.state().in_supercruise);
        assert_eq!(w.state().body.as_deref(), Some("Sol 4"));
    }

    #[test]
    fn music_track_is_recorded() {
        let mut w = watcher();
        w.apply_line(r#"{"event":"Music","MusicTrack":"Exploration"}"#);
        assert_eq!(w.state().music_track.as_deref(), Some("Exploration"));
        assert!(w.state().music_playing());
    }

    #[test]
    fn no_track_does_not_count_as_music() {
        let mut w = watcher();
        assert!(!w.state().music_playing(), "unknown is not playing");
        w.apply_line(r#"{"event":"Music","MusicTrack":"NoTrack"}"#);
        assert!(!w.state().music_playing());
        w.apply_line(r#"{"event":"Music","MusicTrack":"Supercruise"}"#);
        assert!(w.state().music_playing());
    }

    #[test]
    fn shutdown_marks_the_game_as_gone() {
        let mut w = watcher();
        w.apply_line(r#"{"event":"Fileheader"}"#);
        assert!(w.state().game_running);
        w.apply_line(r#"{"event":"SupercruiseEntry"}"#);
        w.apply_line(r#"{"event":"Shutdown"}"#);
        assert!(!w.state().game_running);
        assert!(!w.state().in_supercruise);
    }

    #[test]
    fn unknown_and_malformed_lines_are_ignored() {
        let mut w = watcher();
        assert!(!w.apply_line(""));
        assert!(!w.apply_line("not json at all"));
        assert!(!w.apply_line("{}"));
        assert!(!w.apply_line(r#"{"event":"SomeEventAddedInAFutureUpdate","x":1}"#));
        assert_eq!(w.state(), &GameState::default());
    }

    #[test]
    fn partial_coordinates_do_not_produce_a_bogus_position() {
        let mut w = watcher();
        w.apply_line(r#"{"event":"FSDJump","StarSystem":"X","StarPos":[1.0,2.0]}"#);
        assert_eq!(w.state().star_pos, None);
        w.apply_line(r#"{"event":"FSDJump","StarSystem":"Y","StarPos":[1.0,"bad",3.0]}"#);
        assert_eq!(w.state().star_pos, None);
        assert_eq!(w.state().star_system.as_deref(), Some("Y"));
    }

    #[test]
    fn a_missing_directory_is_survivable() {
        let mut w = JournalWatcher::new("/definitely/not/here");
        assert_eq!(w.poll(), 0);
        assert_eq!(w.poll(), 0, "repeated polls must stay quiet");
        assert_eq!(w.state(), &GameState::default());
    }

    /// Exercises the real file-tailing path, including rotation.
    #[test]
    fn tails_appended_lines_and_follows_rotation() {
        let dir = std::env::temp_dir().join(format!("ed-compass-journal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let first = dir.join("Journal.2311-08-13T140000.01.log");
        std::fs::write(
            &first,
            "{\"event\":\"Fileheader\"}\n{\"event\":\"Location\",\"StarSystem\":\"Sol\",\"StarPos\":[0.0,0.0,0.0]}\n",
        )
        .unwrap();

        let mut w = JournalWatcher::new(&dir);
        assert_eq!(w.poll(), 2);
        assert_eq!(w.state().star_system.as_deref(), Some("Sol"));

        // Nothing new.
        assert_eq!(w.poll(), 0);

        // Append, including a half-written trailing line the game has not
        // finished flushing.
        let mut content = std::fs::read_to_string(&first).unwrap();
        content.push_str(
            "{\"event\":\"FSDJump\",\"StarSystem\":\"Merope\",\"StarPos\":[-78.6,-149.6,-340.5]}\n{\"event\":\"FSD",
        );
        std::fs::write(&first, &content).unwrap();
        assert_eq!(w.poll(), 1);
        assert_eq!(w.state().star_system.as_deref(), Some("Merope"));

        // A new log file appears — the game restarted.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let second = dir.join("Journal.2311-08-13T150000.01.log");
        std::fs::write(
            &second,
            "{\"event\":\"Location\",\"StarSystem\":\"Shinrarta Dezhra\",\"StarPos\":[55.7,17.6,27.2]}\n",
        )
        .unwrap();
        assert_eq!(w.poll(), 1);
        assert_eq!(w.state().star_system.as_deref(), Some("Shinrarta Dezhra"));
        assert_eq!(w.current_file(), Some(second.as_path()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_truncated_file_is_re_read_rather_than_read_past_the_end() {
        let dir = std::env::temp_dir().join(format!("ed-compass-trunc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Journal.2311-01-01T000000.01.log");

        std::fs::write(
            &path,
            "{\"event\":\"Location\",\"StarSystem\":\"Aaa\"}\n".repeat(5),
        )
        .unwrap();
        let mut w = JournalWatcher::new(&dir);
        assert_eq!(w.poll(), 5);

        std::fs::write(&path, "{\"event\":\"Location\",\"StarSystem\":\"Bbb\"}\n").unwrap();
        assert_eq!(w.poll(), 1);
        assert_eq!(w.state().star_system.as_deref(), Some("Bbb"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
