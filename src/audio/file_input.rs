//! Offline WAV and FLAC input.
//!
//! Runs recorded material through the identical analysis chain as live capture,
//! which is how reference recordings — including the community's Landscape
//! Signal captures — get compared against what the tool hears live.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::audio::{SampleFormat, StreamFormat};

/// A decoded file, replayed as interleaved `f32`.
#[derive(Debug)]
pub struct FileSource {
    samples: Vec<f32>,
    format: StreamFormat,
    position: usize,
    /// Replay from the start on reaching the end, so a short reference clip can
    /// still fill the long-term tier for periodicity analysis.
    looping: bool,
}

impl FileSource {
    pub fn format(&self) -> &StreamFormat {
        &self.format
    }

    pub fn total_frames(&self) -> usize {
        self.samples.len() / self.format.channels
    }

    pub fn duration_seconds(&self) -> f64 {
        self.total_frames() as f64 / self.format.sample_rate as f64
    }

    pub fn set_looping(&mut self, looping: bool) {
        self.looping = looping;
    }

    pub fn is_finished(&self) -> bool {
        !self.looping && self.position >= self.samples.len()
    }

    /// Append up to `frames` of interleaved audio. Returns how many frames were
    /// produced — fewer than asked, or zero, at the end of a non-looping file.
    pub fn render(&mut self, frames: usize, out: &mut Vec<f32>) -> usize {
        let channels = self.format.channels;
        let mut produced = 0;
        while produced < frames {
            if self.position >= self.samples.len() {
                if !self.looping || self.samples.is_empty() {
                    break;
                }
                self.position = 0;
            }
            let remaining_frames = (self.samples.len() - self.position) / channels;
            let take = (frames - produced).min(remaining_frames);
            if take == 0 {
                break;
            }
            let end = self.position + take * channels;
            out.extend_from_slice(&self.samples[self.position..end]);
            self.position = end;
            produced += take;
        }
        produced
    }
}

/// Decode a WAV or FLAC file by extension.
pub fn load(path: &Path) -> Result<FileSource> {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    let source = match extension.as_str() {
        "wav" | "wave" => load_wav(path),
        "flac" => load_flac(path),
        other => bail!("unsupported input format {other:?}; expected .wav or .flac"),
    }?;

    if source.samples.is_empty() {
        bail!("{} contains no audio", path.display());
    }
    log::info!(
        "loaded {} — {}, {:.1} s",
        path.display(),
        source.format.describe(),
        source.duration_seconds()
    );
    Ok(source)
}

fn load_wav(path: &Path) -> Result<FileSource> {
    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let spec = reader.spec();
    if spec.channels == 0 {
        bail!("{} declares zero channels", path.display());
    }

    // Normalization matches `format::convert_to_f32`: scale by the full negative
    // range so the most negative code maps to exactly -1.0.
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<std::result::Result<_, _>>()
            .context("decoding float samples")?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 * scale))
                .collect::<std::result::Result<_, _>>()
                .context("decoding integer samples")?
        }
    };

    let sample_format = match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Float, _) => SampleFormat::F32,
        (hound::SampleFormat::Int, 16) => SampleFormat::I16,
        (hound::SampleFormat::Int, 24) => SampleFormat::I24,
        (hound::SampleFormat::Int, _) => SampleFormat::I32,
    };

    Ok(FileSource {
        samples,
        format: StreamFormat::new(spec.sample_rate, spec.channels as usize, 0, sample_format),
        position: 0,
        looping: false,
    })
}

fn load_flac(path: &Path) -> Result<FileSource> {
    let mut reader =
        claxon::FlacReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let info = reader.streaminfo();
    if info.channels == 0 {
        bail!("{} declares zero channels", path.display());
    }

    let scale = 1.0 / (1i64 << (info.bits_per_sample - 1)) as f32;
    let mut samples = Vec::new();
    for sample in reader.samples() {
        samples.push(sample.context("decoding FLAC samples")? as f32 * scale);
    }

    let sample_format = match info.bits_per_sample {
        16 => SampleFormat::I16,
        24 => SampleFormat::I24,
        _ => SampleFormat::I32,
    };

    Ok(FileSource {
        samples,
        format: StreamFormat::new(info.sample_rate, info.channels as usize, 0, sample_format),
        position: 0,
        looping: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ed-compass-{}-{:?}-{name}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    fn write_wav(path: &Path, channels: u16, sample_rate: u32, frames: usize) {
        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..frames {
            for c in 0..channels {
                w.write_sample((i * 10 + c as usize) as f32 / 1000.0)
                    .unwrap();
            }
        }
        w.finalize().unwrap();
    }

    #[test]
    fn loads_a_float_wav_and_reports_its_shape() {
        let path = temp_path("float.wav");
        write_wav(&path, 2, 48_000, 4800);

        let source = load(&path).unwrap();
        assert_eq!(source.format().channels, 2);
        assert_eq!(source.format().sample_rate, 48_000);
        assert_eq!(source.format().sample_format, SampleFormat::F32);
        assert_eq!(source.total_frames(), 4800);
        assert!((source.duration_seconds() - 0.1).abs() < 1e-9);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn loads_an_integer_wav_normalized_to_unit_range() {
        let path = temp_path("int.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for v in [i16::MIN, 0, i16::MAX] {
            w.write_sample(v).unwrap();
        }
        w.finalize().unwrap();

        let mut source = load(&path).unwrap();
        assert_eq!(source.format().sample_format, SampleFormat::I16);
        let mut out = Vec::new();
        assert_eq!(source.render(3, &mut out), 3);
        assert!((out[0] + 1.0).abs() < 1e-6);
        assert!(out[1].abs() < 1e-6);
        assert!(out[2] > 0.999 && out[2] <= 1.0);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rendering_walks_the_file_and_then_stops() {
        let path = temp_path("walk.wav");
        write_wav(&path, 2, 8_000, 100);
        let mut source = load(&path).unwrap();

        let mut out = Vec::new();
        assert_eq!(source.render(60, &mut out), 60);
        assert_eq!(out.len(), 120);
        assert!(!source.is_finished());

        out.clear();
        assert_eq!(source.render(60, &mut out), 40, "clamped to what remains");
        assert!(source.is_finished());

        out.clear();
        assert_eq!(source.render(10, &mut out), 0);
        assert!(out.is_empty());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn looping_replays_from_the_start() {
        let path = temp_path("loop.wav");
        write_wav(&path, 1, 8_000, 10);
        let mut source = load(&path).unwrap();
        source.set_looping(true);

        let mut out = Vec::new();
        assert_eq!(source.render(25, &mut out), 25);
        assert_eq!(out.len(), 25);
        assert!(!source.is_finished(), "a looping source never finishes");
        // Frame 0 of each pass carries the same value.
        assert_eq!(out[0], out[10]);
        assert_eq!(out[0], out[20]);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn interleaving_is_preserved() {
        let path = temp_path("interleave.wav");
        write_wav(&path, 3, 8_000, 4);
        let mut source = load(&path).unwrap();
        let mut out = Vec::new();
        source.render(4, &mut out);
        // Frame 1: channels 0,1,2 => 10, 11, 12 (scaled).
        assert!((out[3] - 0.010).abs() < 1e-6);
        assert!((out[4] - 0.011).abs() < 1e-6);
        assert!((out[5] - 0.012).abs() < 1e-6);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unsupported_and_missing_files_fail_with_a_clear_message() {
        let err = load(Path::new("/tmp/whatever.mp3"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unsupported input format"), "{err}");

        let missing = temp_path("does-not-exist.wav");
        assert!(load(&missing).is_err());
    }

    #[test]
    fn an_empty_file_is_rejected_rather_than_analyzed() {
        let path = temp_path("empty.wav");
        write_wav(&path, 2, 8_000, 0);
        let err = load(&path).unwrap_err().to_string();
        assert!(err.contains("contains no audio"), "{err}");
        let _ = std::fs::remove_file(&path);
    }
}
