//! The shapes the audio pipeline is built from: decoded packets, the sink
//! they are written to, and the volume the output applies.
//!
//! The pipeline runs at one fixed format, stereo at [`SAMPLE_RATE`]. The
//! player converts whatever the server streams to it before the equalizer,
//! the visualisers and the output see a sample, so none of them has to care
//! what the file was.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The rate the equalizer, the visualisers and the output queue run at.
pub const SAMPLE_RATE: u32 = 44_100;
pub const NUM_CHANNELS: u8 = 2;

/// Decibels between the quietest audible volume step and full volume.
const VOLUME_DB_RANGE: f64 = 60.0;

#[derive(Debug, thiserror::Error)]
pub enum SinkError {
    #[error("{0}")]
    ConnectionRefused(String),
    #[error("{0}")]
    NotConnected(String),
    #[error("{0}")]
    OnWrite(String),
}

pub type SinkResult<T> = Result<T, SinkError>;

/// Interleaved stereo samples at [`SAMPLE_RATE`].
pub enum AudioPacket {
    Samples(Vec<f64>),
}

impl AudioPacket {
    pub fn samples(&self) -> SinkResult<&[f64]> {
        match self {
            Self::Samples(samples) => Ok(samples),
        }
    }
}

/// Narrows the pipeline's samples to what the device takes.
#[derive(Default)]
pub struct Converter;

impl Converter {
    pub fn f64_to_f32(&mut self, samples: &[f64]) -> Vec<f32> {
        samples.iter().map(|sample| *sample as f32).collect()
    }
}

pub trait Sink {
    fn start(&mut self) -> SinkResult<()>;
    fn stop(&mut self) -> SinkResult<()>;
    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()>;
}

pub trait VolumeGetter {
    /// The gain the output multiplies samples by, from 0 to 1.
    fn attenuation_factor(&self) -> f64;
}

/// Volume applied somewhere else, or not at all.
pub struct NoOpVolume;

impl VolumeGetter for NoOpVolume {
    fn attenuation_factor(&self) -> f64 {
        1.0
    }
}

/// The volume slider's level, shared between the player and the output.
///
/// A plain 60 dB logarithmic curve puts half the slider below -30 dB and
/// every level anyone wants in its top quarter. The cubic curve reaches
/// -16 dB at the middle and -7 dB at three quarters, spreading the useful
/// range across the slider.
#[derive(Clone)]
pub struct SoftVolume(Arc<AtomicU64>);

impl SoftVolume {
    pub fn new(volume: u16) -> Self {
        Self(Arc::new(AtomicU64::new(Self::factor(volume).to_bits())))
    }

    pub fn set(&self, volume: u16) {
        self.0
            .store(Self::factor(volume).to_bits(), Ordering::Relaxed);
    }

    fn factor(volume: u16) -> f64 {
        if volume == 0 {
            return 0.0;
        }
        let normalized = f64::from(volume) / f64::from(u16::MAX);
        let min_norm = (1.0 / 10f64.powf(VOLUME_DB_RANGE / 20.0)).cbrt();
        (normalized * (1.0 - min_norm) + min_norm).powi(3)
    }
}

impl VolumeGetter for SoftVolume {
    fn attenuation_factor(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_volume_curve_runs_from_silence_to_unity() {
        assert_eq!(SoftVolume::factor(0), 0.0);
        assert!((SoftVolume::factor(u16::MAX) - 1.0).abs() < 1e-9);
        let middle = 20.0 * SoftVolume::factor(u16::MAX / 2).log10();
        assert!((-17.0..-15.0).contains(&middle), "{middle} dB at half");
    }
}
