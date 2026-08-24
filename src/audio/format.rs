//! Stream format handling: sample conversion, channel layout, and the mapping
//! from Windows speaker positions to azimuths.
//!
//! Azimuth convention used throughout the application:
//!
//! ```text
//!            0°  (ship's nose)
//!             │
//!   -90° ─────┼───── +90°
//!             │
//!           ±180°
//! ```
//!
//! Negative is to port, positive to starboard, range `(-180, 180]`.

/// Interleaved sample encodings WASAPI can hand us in shared mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    F32,
    I16,
    /// Packed 24-bit, three bytes per sample.
    I24,
    I32,
}

impl SampleFormat {
    pub fn bytes_per_sample(self) -> usize {
        match self {
            SampleFormat::F32 | SampleFormat::I32 => 4,
            SampleFormat::I24 => 3,
            SampleFormat::I16 => 2,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SampleFormat::F32 => "F32",
            SampleFormat::I16 => "S16",
            SampleFormat::I24 => "S24",
            SampleFormat::I32 => "S32",
        }
    }
}

/// Choose a sample format from the container size and whether the stream is
/// float.
///
/// The container size — `nBlockAlign / nChannels` — is what matters, not
/// `wBitsPerSample`: 24 valid bits inside a 4-byte container is a 32-bit stream
/// whose low byte is zero, and reading it as packed 24-bit would desync every
/// sample after the first.
pub fn classify(container_bytes: usize, is_float: bool) -> Option<SampleFormat> {
    match (container_bytes, is_float) {
        (4, true) => Some(SampleFormat::F32),
        (4, false) => Some(SampleFormat::I32),
        (3, false) => Some(SampleFormat::I24),
        (2, false) => Some(SampleFormat::I16),
        _ => None,
    }
}

/// Convert a raw interleaved byte buffer to normalized `f32`, appending to `out`.
///
/// Integer formats are scaled by the full negative range (2^(n-1)), which maps
/// the most negative code to exactly -1.0 and full-scale positive to slightly
/// under +1.0. That is the standard convention and keeps round-trips exact.
///
/// Trailing bytes that do not form a whole sample are ignored; a short final
/// packet is a device quirk, not something worth failing capture over.
pub fn convert_to_f32(raw: &[u8], format: SampleFormat, out: &mut Vec<f32>) {
    let count = raw.len() / format.bytes_per_sample();
    out.reserve(count);
    match format {
        SampleFormat::F32 => {
            for c in raw.as_chunks::<4>().0 {
                out.push(f32::from_le_bytes(*c));
            }
        }
        SampleFormat::I16 => {
            for c in raw.as_chunks::<2>().0 {
                let v = i16::from_le_bytes(*c);
                out.push(v as f32 / 32_768.0);
            }
        }
        SampleFormat::I24 => {
            for c in raw.as_chunks::<3>().0 {
                // Sign-extend by landing the 24 bits in the high bytes of an i32.
                let v = i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8;
                out.push(v as f32 / 8_388_608.0);
            }
        }
        SampleFormat::I32 => {
            for c in raw.as_chunks::<4>().0 {
                let v = i32::from_le_bytes(*c);
                out.push(v as f32 / 2_147_483_648.0);
            }
        }
    }
}

// Windows `dwChannelMask` bits (ksmedia.h). Defined here rather than pulled from
// the `windows` crate so the layout logic builds and tests on any platform.
pub const SPEAKER_FRONT_LEFT: u32 = 0x1;
pub const SPEAKER_FRONT_RIGHT: u32 = 0x2;
pub const SPEAKER_FRONT_CENTER: u32 = 0x4;
pub const SPEAKER_LOW_FREQUENCY: u32 = 0x8;
pub const SPEAKER_BACK_LEFT: u32 = 0x10;
pub const SPEAKER_BACK_RIGHT: u32 = 0x20;
pub const SPEAKER_FRONT_LEFT_OF_CENTER: u32 = 0x40;
pub const SPEAKER_FRONT_RIGHT_OF_CENTER: u32 = 0x80;
pub const SPEAKER_BACK_CENTER: u32 = 0x100;
pub const SPEAKER_SIDE_LEFT: u32 = 0x200;
pub const SPEAKER_SIDE_RIGHT: u32 = 0x400;
pub const SPEAKER_TOP_CENTER: u32 = 0x800;
pub const SPEAKER_TOP_FRONT_LEFT: u32 = 0x1000;
pub const SPEAKER_TOP_FRONT_CENTER: u32 = 0x2000;
pub const SPEAKER_TOP_FRONT_RIGHT: u32 = 0x4000;
pub const SPEAKER_TOP_BACK_LEFT: u32 = 0x8000;
pub const SPEAKER_TOP_BACK_CENTER: u32 = 0x10000;
pub const SPEAKER_TOP_BACK_RIGHT: u32 = 0x20000;

pub const MASK_STEREO: u32 = SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT;
pub const MASK_QUAD: u32 = MASK_STEREO | SPEAKER_BACK_LEFT | SPEAKER_BACK_RIGHT;
pub const MASK_5_1: u32 = MASK_STEREO
    | SPEAKER_FRONT_CENTER
    | SPEAKER_LOW_FREQUENCY
    | SPEAKER_BACK_LEFT
    | SPEAKER_BACK_RIGHT;
pub const MASK_7_1: u32 = MASK_5_1 | SPEAKER_SIDE_LEFT | SPEAKER_SIDE_RIGHT;

/// One channel's role in the layout.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChannelInfo {
    /// Azimuth in degrees, or `None` for channels that carry no usable
    /// direction — LFE (no directional content) and height channels (the
    /// direction finder is horizontal-only).
    pub azimuth_deg: Option<f32>,
    pub name: &'static str,
}

/// Speaker azimuths, in `dwChannelMask` bit order — which is the order WASAPI
/// interleaves them.
///
/// Back speakers are placed at ±110° in a 5.1 layout and ±135° in 7.1. The mask
/// does not distinguish the two, but the presence of side channels does, and the
/// difference is large enough to matter for a bearing estimate.
pub fn channel_layout(mask: u32, channels: usize) -> Vec<ChannelInfo> {
    if mask == 0 {
        return default_layout(channels);
    }

    let has_sides = mask & (SPEAKER_SIDE_LEFT | SPEAKER_SIDE_RIGHT) != 0;
    let back = if has_sides { 135.0 } else { 110.0 };

    let table: [(u32, Option<f32>, &'static str); 18] = [
        (SPEAKER_FRONT_LEFT, Some(-30.0), "FL"),
        (SPEAKER_FRONT_RIGHT, Some(30.0), "FR"),
        (SPEAKER_FRONT_CENTER, Some(0.0), "FC"),
        (SPEAKER_LOW_FREQUENCY, None, "LFE"),
        (SPEAKER_BACK_LEFT, Some(-back), "BL"),
        (SPEAKER_BACK_RIGHT, Some(back), "BR"),
        (SPEAKER_FRONT_LEFT_OF_CENTER, Some(-15.0), "FLC"),
        (SPEAKER_FRONT_RIGHT_OF_CENTER, Some(15.0), "FRC"),
        (SPEAKER_BACK_CENTER, Some(180.0), "BC"),
        (SPEAKER_SIDE_LEFT, Some(-90.0), "SL"),
        (SPEAKER_SIDE_RIGHT, Some(90.0), "SR"),
        (SPEAKER_TOP_CENTER, None, "TC"),
        (SPEAKER_TOP_FRONT_LEFT, None, "TFL"),
        (SPEAKER_TOP_FRONT_CENTER, None, "TFC"),
        (SPEAKER_TOP_FRONT_RIGHT, None, "TFR"),
        (SPEAKER_TOP_BACK_LEFT, None, "TBL"),
        (SPEAKER_TOP_BACK_CENTER, None, "TBC"),
        (SPEAKER_TOP_BACK_RIGHT, None, "TBR"),
    ];

    let mut layout: Vec<ChannelInfo> = table
        .iter()
        .filter(|(bit, _, _)| mask & bit != 0)
        .map(|&(_, azimuth_deg, name)| ChannelInfo { azimuth_deg, name })
        .take(channels)
        .collect();

    // A mask that names fewer speakers than the stream has channels is
    // malformed; treat the surplus as directionless rather than dropping it.
    while layout.len() < channels {
        layout.push(ChannelInfo {
            azimuth_deg: None,
            name: "?",
        });
    }
    layout
}

/// Layout for a stream that declares no mask, inferred from channel count.
fn default_layout(channels: usize) -> Vec<ChannelInfo> {
    match channels {
        1 => vec![ChannelInfo {
            azimuth_deg: Some(0.0),
            name: "M",
        }],
        2 => channel_layout(MASK_STEREO, 2),
        4 => channel_layout(MASK_QUAD, 4),
        6 => channel_layout(MASK_5_1, 6),
        8 => channel_layout(MASK_7_1, 8),
        n => (0..n)
            .map(|_| ChannelInfo {
                azimuth_deg: None,
                name: "?",
            })
            .collect(),
    }
}

/// Human-readable layout name for the UI.
pub fn layout_name(mask: u32, channels: usize) -> &'static str {
    match (mask, channels) {
        (_, 1) => "mono",
        (MASK_STEREO, 2) | (0, 2) => "stereo",
        (MASK_QUAD, 4) | (0, 4) => "quad",
        (MASK_5_1, 6) | (0, 6) => "5.1",
        (MASK_7_1, 8) | (0, 8) => "7.1",
        _ => "custom",
    }
}

/// Average all channels into mono, appending to `out`.
///
/// Used only for detection. Direction finding always works on the individual
/// channels, because this operation is exactly what destroys the bearing.
pub fn downmix_mono(interleaved: &[f32], channels: usize, out: &mut Vec<f32>) {
    assert!(channels > 0, "downmix needs at least one channel");
    let scale = 1.0 / channels as f32;
    out.reserve(interleaved.len() / channels);
    for frame in interleaved.chunks_exact(channels) {
        out.push(frame.iter().sum::<f32>() * scale);
    }
}

/// Deinterleave into `channels` separate buffers, appending to each.
pub fn deinterleave(interleaved: &[f32], out: &mut [Vec<f32>]) {
    let channels = out.len();
    assert!(channels > 0, "deinterleave needs at least one channel");
    for frame in interleaved.chunks_exact(channels) {
        for (c, sample) in frame.iter().enumerate() {
            out[c].push(*sample);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-6, "{a} != {b}");
    }

    #[test]
    fn f32_passes_through_unchanged() {
        let raw: Vec<u8> = [0.5f32, -0.25, 1.0]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let mut out = Vec::new();
        convert_to_f32(&raw, SampleFormat::F32, &mut out);
        assert_eq!(out, vec![0.5, -0.25, 1.0]);
    }

    #[test]
    fn i16_hits_the_rails_exactly() {
        let raw: Vec<u8> = [i16::MIN, 0, i16::MAX]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let mut out = Vec::new();
        convert_to_f32(&raw, SampleFormat::I16, &mut out);
        approx(out[0], -1.0);
        approx(out[1], 0.0);
        assert!(out[2] > 0.9999 && out[2] < 1.0);
    }

    #[test]
    fn i24_sign_extends() {
        // -1 is 0xFFFFFF, +full-scale-1 is 0x7FFFFF, and 0x800000 is -1.0.
        let raw = vec![
            0xFF, 0xFF, 0xFF, // -1
            0x00, 0x00, 0x00, // 0
            0xFF, 0xFF, 0x7F, // 8388607
            0x00, 0x00, 0x80, // -8388608
        ];
        let mut out = Vec::new();
        convert_to_f32(&raw, SampleFormat::I24, &mut out);
        approx(out[0], -1.0 / 8_388_608.0);
        approx(out[1], 0.0);
        assert!(out[2] > 0.9999 && out[2] < 1.0);
        approx(out[3], -1.0);
    }

    #[test]
    fn i32_scales_to_unit_range() {
        let raw: Vec<u8> = [i32::MIN, 0, i32::MAX]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let mut out = Vec::new();
        convert_to_f32(&raw, SampleFormat::I32, &mut out);
        approx(out[0], -1.0);
        approx(out[1], 0.0);
        // i32::MAX / 2^31 is 0.9999999995, which is not representable in f32 and
        // rounds to exactly 1.0. That is a property of the destination type, not
        // an overflow — the value never exceeds full scale.
        assert!((0.9999..=1.0).contains(&out[2]), "got {}", out[2]);
    }

    #[test]
    fn ragged_tail_bytes_are_ignored() {
        let mut out = Vec::new();
        convert_to_f32(&[0, 0, 0, 0, 1], SampleFormat::F32, &mut out);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn classification_follows_the_container_not_the_valid_bits() {
        assert_eq!(classify(4, true), Some(SampleFormat::F32));
        assert_eq!(classify(4, false), Some(SampleFormat::I32));
        assert_eq!(classify(3, false), Some(SampleFormat::I24));
        assert_eq!(classify(2, false), Some(SampleFormat::I16));
        // 24 valid bits in a 4-byte container is a 32-bit stream.
        assert_eq!(classify(4, false), Some(SampleFormat::I32));
        // Nothing sane produces these.
        assert_eq!(classify(1, false), None);
        assert_eq!(classify(8, true), None);
        assert_eq!(classify(3, true), None);
        assert_eq!(classify(0, false), None);
    }

    #[test]
    fn stereo_layout_is_plus_minus_thirty() {
        let l = channel_layout(MASK_STEREO, 2);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].azimuth_deg, Some(-30.0));
        assert_eq!(l[1].azimuth_deg, Some(30.0));
    }

    #[test]
    fn five_one_is_in_mask_bit_order_with_lfe_excluded() {
        let l = channel_layout(MASK_5_1, 6);
        let names: Vec<&str> = l.iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["FL", "FR", "FC", "LFE", "BL", "BR"]);
        assert_eq!(l[3].azimuth_deg, None, "LFE carries no direction");
        // No sides present, so backs sit at the 5.1 position.
        assert_eq!(l[4].azimuth_deg, Some(-110.0));
        assert_eq!(l[5].azimuth_deg, Some(110.0));
    }

    #[test]
    fn seven_one_moves_the_backs_and_adds_sides() {
        let l = channel_layout(MASK_7_1, 8);
        let names: Vec<&str> = l.iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR"]);
        assert_eq!(l[4].azimuth_deg, Some(-135.0));
        assert_eq!(l[5].azimuth_deg, Some(135.0));
        assert_eq!(l[6].azimuth_deg, Some(-90.0));
        assert_eq!(l[7].azimuth_deg, Some(90.0));
    }

    #[test]
    fn height_channels_are_not_used_for_bearing() {
        let mask = MASK_STEREO | SPEAKER_TOP_FRONT_LEFT | SPEAKER_TOP_FRONT_RIGHT;
        let l = channel_layout(mask, 4);
        assert_eq!(l[2].azimuth_deg, None);
        assert_eq!(l[3].azimuth_deg, None);
    }

    #[test]
    fn zero_mask_falls_back_to_channel_count() {
        assert_eq!(channel_layout(0, 2), channel_layout(MASK_STEREO, 2));
        assert_eq!(channel_layout(0, 8), channel_layout(MASK_7_1, 8));
        assert_eq!(channel_layout(0, 1)[0].azimuth_deg, Some(0.0));
    }

    #[test]
    fn mask_naming_fewer_speakers_than_channels_pads_safely() {
        let l = channel_layout(MASK_STEREO, 4);
        assert_eq!(l.len(), 4);
        assert_eq!(l[2].azimuth_deg, None);
        assert_eq!(l[3].azimuth_deg, None);
    }

    #[test]
    fn mask_naming_more_speakers_than_channels_truncates() {
        let l = channel_layout(MASK_7_1, 2);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].name, "FL");
        assert_eq!(l[1].name, "FR");
    }

    #[test]
    fn unusual_channel_count_yields_no_bearings() {
        let l = channel_layout(0, 3);
        assert_eq!(l.len(), 3);
        assert!(l.iter().all(|c| c.azimuth_deg.is_none()));
    }

    #[test]
    fn layout_names_cover_the_common_cases() {
        assert_eq!(layout_name(MASK_STEREO, 2), "stereo");
        assert_eq!(layout_name(MASK_5_1, 6), "5.1");
        assert_eq!(layout_name(MASK_7_1, 8), "7.1");
        assert_eq!(layout_name(0, 1), "mono");
        assert_eq!(layout_name(0x1234, 3), "custom");
    }

    #[test]
    fn downmix_averages_channels() {
        let mut out = Vec::new();
        downmix_mono(&[1.0, 0.0, -1.0, 1.0], 2, &mut out);
        assert_eq!(out, vec![0.5, 0.0]);
    }

    #[test]
    fn deinterleave_splits_channels() {
        let mut out = vec![Vec::new(), Vec::new(), Vec::new()];
        deinterleave(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], &mut out);
        assert_eq!(out[0], vec![1.0, 4.0]);
        assert_eq!(out[1], vec![2.0, 5.0]);
        assert_eq!(out[2], vec![3.0, 6.0]);
    }
}
