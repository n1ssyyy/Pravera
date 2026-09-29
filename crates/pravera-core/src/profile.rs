use serde::{Deserialize, Serialize};

use crate::media::PixelFormat;

/// How the pipeline resolves the standing conflict between picture quality and
/// responsiveness.
///
/// These are not cosmetic presets: each selects a different chroma layout,
/// rate-control strategy and loss policy all the way down the stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum QualityProfile {
    /// Crisp text above all. 4:4:4 where the encoder supports it, damage-driven
    /// frames, and no frames at all while the screen is still.
    Quality,
    /// Retunes continuously from measured RTT, loss and motion. The default,
    /// and the right answer for almost everyone.
    #[default]
    Adaptive,
    /// Latency above all: subsampled chroma, fixed cadence, no B-frames, and
    /// loss absorbed by FEC rather than retransmission.
    Latency,
}

impl QualityProfile {
    pub const fn name(self) -> &'static str {
        match self {
            QualityProfile::Quality => "Quality",
            QualityProfile::Adaptive => "Adaptive",
            QualityProfile::Latency => "Latency",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            QualityProfile::Quality => "Sharpest text. Best for reading and admin work.",
            QualityProfile::Adaptive => "Balances sharpness and responsiveness automatically.",
            QualityProfile::Latency => "Fastest response. Best for gaming.",
        }
    }

    /// Preferred encoder input format. Adaptive starts subsampled and is free
    /// to switch to 4:4:4 once it observes a still screen on a fat link.
    pub const fn preferred_pixel_format(self) -> PixelFormat {
        match self {
            QualityProfile::Quality => PixelFormat::Yuv444,
            QualityProfile::Adaptive | QualityProfile::Latency => PixelFormat::Nv12,
        }
    }

    /// Preferred audio codec, on the same logic as the picture.
    ///
    /// Quality spends the bandwidth: 1.5 Mbps of untouched PCM next to a video
    /// stream several times that size is not the thing anyone will notice.
    /// The other two compress, because on a link where the picture is already
    /// being rationed, audio should not be the reason a frame is dropped.
    pub const fn preferred_audio_codec(self) -> crate::AudioCodec {
        match self {
            QualityProfile::Quality => crate::AudioCodec::Pcm16,
            QualityProfile::Adaptive | QualityProfile::Latency => crate::AudioCodec::Adpcm4,
        }
    }

    /// Whether a lost packet may be retransmitted. Latency never waits for one;
    /// it relies on FEC and, past a threshold, asks for a fresh keyframe.
    pub const fn allows_retransmission(self) -> bool {
        matches!(self, QualityProfile::Quality | QualityProfile::Adaptive)
    }

    /// Encode only changed regions, and emit nothing while the screen is still.
    /// Latency holds a fixed cadence instead, because a game is never still and
    /// a predictable frame clock is worth more than the saved bits.
    pub const fn uses_damage_regions(self) -> bool {
        matches!(self, QualityProfile::Quality | QualityProfile::Adaptive)
    }

    /// Inclusive frame-rate bounds the rate controller may pick from.
    pub const fn fps_range(self) -> (u32, u32) {
        match self {
            QualityProfile::Quality => (0, 60),
            QualityProfile::Adaptive => (0, 144),
            QualityProfile::Latency => (60, 144),
        }
    }

    /// B-frames improve compression but require reordering, which costs at
    /// least one frame of latency. Only Quality can afford them.
    pub const fn allows_b_frames(self) -> bool {
        matches!(self, QualityProfile::Quality)
    }

    pub const ALL: [QualityProfile; 3] = [
        QualityProfile::Quality,
        QualityProfile::Adaptive,
        QualityProfile::Latency,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_is_the_default() {
        assert_eq!(QualityProfile::default(), QualityProfile::Adaptive);
    }

    #[test]
    fn latency_never_retransmits_or_reorders() {
        let p = QualityProfile::Latency;
        assert!(!p.allows_retransmission());
        assert!(!p.allows_b_frames());
        assert!(!p.uses_damage_regions());
    }

    #[test]
    fn latency_holds_a_floor_on_frame_rate() {
        // A game must never drop to 0 fps the way an idle desktop may.
        assert_eq!(QualityProfile::Latency.fps_range().0, 60);
        assert_eq!(QualityProfile::Quality.fps_range().0, 0);
    }

    #[test]
    fn only_quality_asks_for_full_chroma() {
        assert_eq!(
            QualityProfile::Quality.preferred_pixel_format(),
            PixelFormat::Yuv444
        );
        assert!(QualityProfile::Latency
            .preferred_pixel_format()
            .is_chroma_subsampled());
    }
}
