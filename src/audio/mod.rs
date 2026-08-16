//! Audio capture, buffering, and stream format handling.

pub mod capture;
pub mod device;
pub mod file_input;
pub mod format;
pub mod ring_buffer;
pub mod synthetic;

pub use format::{ChannelInfo, SampleFormat};
pub use ring_buffer::PcmRing;

/// The shape of a live stream, as negotiated with the endpoint.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamFormat {
    pub sample_rate: u32,
    pub channels: usize,
    /// Windows `dwChannelMask`; 0 when the stream declares none.
    pub channel_mask: u32,
    pub sample_format: SampleFormat,
}

impl StreamFormat {
    pub fn new(
        sample_rate: u32,
        channels: usize,
        channel_mask: u32,
        sample_format: SampleFormat,
    ) -> Self {
        Self {
            sample_rate,
            channels,
            channel_mask,
            sample_format,
        }
    }

    pub fn layout(&self) -> Vec<ChannelInfo> {
        format::channel_layout(self.channel_mask, self.channels)
    }

    pub fn layout_name(&self) -> &'static str {
        format::layout_name(self.channel_mask, self.channels)
    }

    /// How many channels carry a usable bearing. Below two, direction finding
    /// is impossible and the UI says so rather than showing a fake compass.
    pub fn directional_channels(&self) -> usize {
        self.layout()
            .iter()
            .filter(|c| c.azimuth_deg.is_some())
            .count()
    }

    pub fn seconds_to_frames(&self, seconds: f32) -> usize {
        (seconds * self.sample_rate as f32).ceil().max(0.0) as usize
    }

    pub fn frames_to_seconds(&self, frames: u64) -> f64 {
        frames as f64 / self.sample_rate as f64
    }

    /// One line for the UI header and the log.
    pub fn describe(&self) -> String {
        format!(
            "{} Hz · {} ch ({}) · {}",
            self.sample_rate,
            self.channels,
            self.layout_name(),
            self.sample_format.label()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use format::{MASK_7_1, MASK_STEREO};

    #[test]
    fn describes_a_seven_one_stream() {
        let f = StreamFormat::new(48_000, 8, MASK_7_1, SampleFormat::F32);
        assert_eq!(f.describe(), "48000 Hz · 8 ch (7.1) · F32");
        // Eight channels, but LFE carries no bearing.
        assert_eq!(f.directional_channels(), 7);
    }

    #[test]
    fn stereo_has_two_directional_channels() {
        let f = StreamFormat::new(44_100, 2, MASK_STEREO, SampleFormat::I16);
        assert_eq!(f.directional_channels(), 2);
        assert_eq!(f.layout_name(), "stereo");
    }

    #[test]
    fn frame_and_second_conversions_round_trip() {
        let f = StreamFormat::new(48_000, 2, MASK_STEREO, SampleFormat::F32);
        assert_eq!(f.seconds_to_frames(150.0), 7_200_000);
        assert!((f.frames_to_seconds(7_200_000) - 150.0).abs() < 1e-9);
        assert_eq!(f.seconds_to_frames(0.0), 0);
    }
}
