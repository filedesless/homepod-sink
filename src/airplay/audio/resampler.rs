//! High-quality audio resampling for AirPlay 2.
//!
//! Sinc-based resampler with TPDF dithering, used to convert PipeWire's
//! capture rate (or a file's native rate) to whatever AirPlay's ALAC stream
//! needs (44.1kHz).

use crate::airplay::core::error::Result;
use rubato::{
    Resampler as RubatoResampler, SincFixedIn, SincInterpolationParameters,
    SincInterpolationType, WindowFunction,
};
use tracing::{debug, info};

/// Default chunk size for resampling (frames per period).
pub const DEFAULT_CHUNK_SIZE: usize = 1024;

/// Sinc interpolation parameters for mastering-quality resampling.
///
/// - `sinc_len: 512` - Mastering quality (longer = higher quality)
/// - `f_cutoff: 0.95` - Good HF preservation
/// - `interpolation: Cubic` - Cubic interpolation between sinc points
/// - `oversampling_factor: 256` - Higher = better quality
/// - `window: BlackmanHarris2` - Excellent stopband attenuation (-107 dB)
const SINC_PARAMS: SincInterpolationParameters = SincInterpolationParameters {
    sinc_len: 512,
    f_cutoff: 0.95,
    interpolation: SincInterpolationType::Cubic,
    oversampling_factor: 256,
    window: WindowFunction::BlackmanHarris2,
};

/// High-quality sinc-based resampler with TPDF dithering.
///
/// `SincFixedIn` requires *exactly* `chunk_size` input frames per call and
/// silently truncates/discards anything beyond that (per-call, not just at
/// EOF) rather than erroring, so callers can't just hand it arbitrary-sized
/// buffers (e.g. one MP3-decoded frame, which is 1152 samples/channel and
/// doesn't line up with the default 1024-frame chunk). This wrapper buffers
/// input internally and only calls into the inner resampler with exact
/// chunk-sized slices, carrying over any remainder to the next call.
pub struct Resampler {
    inner: SincFixedIn<f32>,
    channels: usize,
    chunk_size: usize,
    /// Buffered input samples per channel, not yet consumed by `inner`.
    input_buffer: Vec<Vec<f32>>,
}

impl Resampler {
    /// Create and prime a new resampler.
    ///
    /// The resampler is automatically primed with silence to prevent
    /// startup distortion.
    pub fn new(source_rate: u32, target_rate: u32, channels: u8) -> Result<Self> {
        Self::with_chunk_size(source_rate, target_rate, channels, DEFAULT_CHUNK_SIZE)
    }

    /// Create a resampler with a custom chunk size.
    pub fn with_chunk_size(
        source_rate: u32,
        target_rate: u32,
        channels: u8,
        chunk_size: usize,
    ) -> Result<Self> {
        let resample_ratio = target_rate as f64 / source_rate as f64;
        let channels_usize = channels as usize;

        let mut inner = SincFixedIn::<f32>::new(
            resample_ratio,
            2.0, // Max relative ratio deviation
            SINC_PARAMS,
            chunk_size,
            channels_usize,
        )
        .map_err(|e| {
            crate::airplay::core::error::StreamingError::Encoding(format!(
                "Failed to create resampler: {}",
                e
            ))
        })?;

        // Prime the resampler with silence to fill its internal buffers
        let output_delay = inner.output_delay();
        info!(
            "Resampler created: {}Hz -> {}Hz, {} channels, delay: {} frames ({:.1}ms)",
            source_rate,
            target_rate,
            channels,
            output_delay,
            output_delay as f32 / target_rate as f32 * 1000.0
        );

        // Calculate how many input chunks we need to fully prime the resampler
        let input_frames_needed = inner.input_frames_next();
        let priming_chunks = (SINC_PARAMS.sinc_len / input_frames_needed).max(3);

        let silence_chunk: Vec<Vec<f32>> = (0..channels_usize)
            .map(|_| vec![0.0f32; input_frames_needed])
            .collect();

        for i in 0..priming_chunks {
            match inner.process(&silence_chunk, None) {
                Ok(_) => {
                    debug!("Priming chunk {}/{} processed", i + 1, priming_chunks);
                }
                Err(e) => {
                    debug!("Error during resampler priming: {}", e);
                    break;
                }
            }
        }
        info!("Resampler primed with {} chunks of silence", priming_chunks);

        Ok(Self {
            inner,
            channels: channels_usize,
            chunk_size,
            input_buffer: vec![Vec::new(); channels_usize],
        })
    }

    /// Reset the resampler state (for seeking).
    pub fn reset(&mut self) {
        self.inner.reset();
        for buf in &mut self.input_buffer {
            buf.clear();
        }

        // Re-prime after reset
        let input_frames_needed = self.inner.input_frames_next();
        let priming_chunks = (SINC_PARAMS.sinc_len / input_frames_needed).max(3);

        let silence_chunk: Vec<Vec<f32>> = (0..self.channels)
            .map(|_| vec![0.0f32; input_frames_needed])
            .collect();

        for _ in 0..priming_chunks {
            let _ = self.inner.process(&silence_chunk, None);
        }
        debug!("Resampler reset and re-primed");
    }

    /// Process interleaved i16 samples.
    ///
    /// Converts i16 -> f32, resamples, then converts back to i16 with TPDF dithering.
    pub fn process(&mut self, samples: &[i16]) -> Result<Vec<i16>> {
        if samples.is_empty() {
            return Ok(Vec::new());
        }

        // Deinterleave and convert to f32
        let num_frames = samples.len() / self.channels;
        let mut input_channels: Vec<Vec<f32>> = (0..self.channels)
            .map(|_| Vec::with_capacity(num_frames))
            .collect();

        for frame_idx in 0..num_frames {
            for ch in 0..self.channels {
                let sample = samples[frame_idx * self.channels + ch];
                input_channels[ch].push(sample as f32 / 32768.0);
            }
        }

        // Resample
        let resampled = self.process_f32(&input_channels)?;

        // Interleave with dithering
        Ok(interleave_with_dither(&resampled))
    }

    /// Process deinterleaved f32 samples.
    ///
    /// This is the more efficient path when samples are already in f32 format
    /// (e.g., from ALSA S24 capture).
    ///
    /// Accepts any number of input frames per call (not just exactly
    /// `chunk_size`): input is appended to an internal per-channel buffer,
    /// which is drained in exact `chunk_size` slices through the inner
    /// resampler. Any leftover frames (fewer than `chunk_size`) are carried
    /// over to the next call rather than being discarded, so callers don't
    /// need to pre-align their input to the resampler's chunk size.
    pub fn process_f32(&mut self, channels: &[Vec<f32>]) -> Result<Vec<Vec<f32>>> {
        if channels.is_empty() || channels[0].is_empty() {
            return Ok(Vec::new());
        }

        for (buf, new_samples) in self.input_buffer.iter_mut().zip(channels.iter()) {
            buf.extend_from_slice(new_samples);
        }

        let mut output: Vec<Vec<f32>> = vec![Vec::new(); self.channels];

        while self.input_buffer[0].len() >= self.chunk_size {
            let chunk: Vec<&[f32]> = self
                .input_buffer
                .iter()
                .map(|buf| &buf[..self.chunk_size])
                .collect();

            let resampled = self.inner.process(&chunk, None).map_err(|e| {
                crate::airplay::core::error::StreamingError::Encoding(format!(
                    "Resampling error: {}",
                    e
                ))
            })?;

            for (out, chan) in output.iter_mut().zip(resampled.into_iter()) {
                out.extend(chan);
            }

            for buf in &mut self.input_buffer {
                buf.drain(..self.chunk_size);
            }
        }

        Ok(output)
    }
}

/// Convert f32 sample to i16 with TPDF dithering.
///
/// TPDF (Triangular Probability Density Function) dithering adds
/// triangular-distributed noise to decorrelate quantization error
/// from the signal, reducing audible distortion.
#[inline]
pub fn dither_to_i16(sample: f32) -> i16 {
    // TPDF dithering: add triangular-distributed noise
    let rand1 = fastrand::f32() - 0.5; // Uniform -0.5 to 0.5
    let rand2 = fastrand::f32() - 0.5;
    let tpdf_noise = (rand1 + rand2) / 32768.0; // Triangular, scaled to 1 LSB

    let dithered = sample + tpdf_noise;
    (dithered * 32767.0).clamp(-32768.0, 32767.0) as i16
}

/// Interleave per-channel f32 buffers to i16 with TPDF dithering.
pub fn interleave_with_dither(channels: &[Vec<f32>]) -> Vec<i16> {
    if channels.is_empty() || channels[0].is_empty() {
        return Vec::new();
    }

    let num_frames = channels[0].len();
    let num_channels = channels.len();
    let mut output = Vec::with_capacity(num_frames * num_channels);

    for frame_idx in 0..num_frames {
        for ch in 0..num_channels {
            output.push(dither_to_i16(channels[ch][frame_idx]));
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    /// Generate a stereo sine wave at the given frequency.
    fn generate_sine_wave_f32(frequency: f32, sample_rate: usize, num_frames: usize) -> Vec<Vec<f32>> {
        let mut left = Vec::with_capacity(num_frames);
        let mut right = Vec::with_capacity(num_frames);

        for i in 0..num_frames {
            let t = i as f32 / sample_rate as f32;
            let sample = (2.0 * PI * frequency * t).sin() * 0.8;
            left.push(sample);
            right.push(sample);
        }

        vec![left, right]
    }

    /// Generate interleaved stereo sine wave as i16.
    fn generate_sine_wave_i16(frequency: f32, sample_rate: u32, num_frames: usize) -> Vec<i16> {
        let mut samples = Vec::with_capacity(num_frames * 2);

        for i in 0..num_frames {
            let t = i as f32 / sample_rate as f32;
            let sample = (2.0 * PI * frequency * t).sin() * 0.8;
            let sample_i16 = (sample * 32767.0) as i16;
            samples.push(sample_i16); // Left
            samples.push(sample_i16); // Right
        }

        samples
    }

    /// Calculate RMS of f32 samples.
    fn calculate_rms_f32(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let sum_squares: f32 = samples.iter().map(|s| s * s).sum();
        (sum_squares / samples.len() as f32).sqrt()
    }

    /// Calculate RMS of i16 samples.
    fn calculate_rms_i16(samples: &[i16]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let sum_squares: f64 = samples.iter().map(|&s| (s as f64).powi(2)).sum();
        ((sum_squares / samples.len() as f64).sqrt() / 32767.0) as f32
    }

    mod resampler_creation {
        use super::*;

        #[test]
        fn creates_48k_to_44k_resampler() {
            let resampler = Resampler::new(48000, 44100, 2);
            assert!(resampler.is_ok());
        }

        #[test]
        fn creates_44k_to_48k_resampler() {
            let resampler = Resampler::new(44100, 48000, 2);
            assert!(resampler.is_ok());
        }

        #[test]
        fn creates_mono_resampler() {
            let resampler = Resampler::new(48000, 44100, 1);
            assert!(resampler.is_ok());
        }
    }

    mod chunk_alignment {
        use super::*;

        /// Regression test: `SincFixedIn` requires exactly `chunk_size` input
        /// frames per call and silently truncates/discards anything beyond
        /// that. Real callers (e.g. MP3 decode, which produces 1152-sample
        /// frames at 48kHz) don't naturally align to the resampler's
        /// 1024-frame default chunk size. Before the internal buffering fix,
        /// feeding 1152-frame chunks repeatedly silently dropped ~11% of
        /// every call's samples instead of erroring, corrupting the output
        /// (audible as static/noise) without ever surfacing an error.
        #[test]
        fn does_not_drop_samples_on_misaligned_chunk_sizes() {
            let mut resampler = Resampler::new(48000, 44100, 2).unwrap();

            // Feed input in 1152-frame chunks (MP3 frame size), not aligned
            // to the resampler's default 1024-frame chunk size.
            const MP3_FRAME_SIZE: usize = 1152;
            const NUM_CALLS: usize = 50;
            let mut total_input_frames = 0usize;
            let mut total_output_frames = 0usize;

            for _ in 0..NUM_CALLS {
                let input = generate_sine_wave_i16(440.0, 48000, MP3_FRAME_SIZE);
                total_input_frames += MP3_FRAME_SIZE;
                let output = resampler.process(&input).unwrap();
                total_output_frames += output.len() / 2;
            }

            // Expected output frames given the 44100/48000 resample ratio.
            // If samples were being silently dropped (truncated to 1024 per
            // call instead of buffering the full 1152), total_output_frames
            // would be far lower than this.
            let expected_ratio = 44100.0 / 48000.0;
            let expected_output_frames = (total_input_frames as f64 * expected_ratio) as usize;

            let tolerance = (expected_output_frames as f64 * 0.05) as usize; // 5% tolerance
            assert!(
                total_output_frames + tolerance >= expected_output_frames,
                "Output frames {} too low vs expected ~{} (input {} frames) - samples being dropped?",
                total_output_frames,
                expected_output_frames,
                total_input_frames
            );
        }

        #[test]
        fn carries_over_remainder_across_calls() {
            let mut resampler = Resampler::new(48000, 44100, 2).unwrap();

            // First call: fewer frames than chunk_size (1024) - should buffer
            // internally and produce no output yet.
            let small_input = generate_sine_wave_i16(440.0, 48000, 100);
            let output1 = resampler.process(&small_input).unwrap();
            assert!(output1.is_empty(), "Expected no output before chunk_size frames accumulate");

            // Second call: enough more frames to cross the chunk_size threshold.
            let rest_input = generate_sine_wave_i16(440.0, 48000, 1000);
            let output2 = resampler.process(&rest_input).unwrap();
            assert!(!output2.is_empty(), "Expected output once buffered input crosses chunk_size");
        }
    }

    mod i16_processing {
        use super::*;

        #[test]
        fn resamples_48k_to_44k() {
            let mut resampler = Resampler::new(48000, 44100, 2).unwrap();

            // Generate 1024 frames of 440Hz sine at 48kHz
            let input = generate_sine_wave_i16(440.0, 48000, 1024);

            let output = resampler.process(&input).unwrap();

            // Output should be approximately 44100/48000 * 1024 = ~940 frames
            let output_frames = output.len() / 2;
            assert!(
                output_frames >= 900 && output_frames <= 1000,
                "Expected ~940 frames, got {}",
                output_frames
            );

            // Verify audio is not silent
            let rms = calculate_rms_i16(&output);
            assert!(rms > 0.3, "Output RMS too low: {}", rms);
        }

        #[test]
        fn resamples_44k_to_48k() {
            let mut resampler = Resampler::new(44100, 48000, 2).unwrap();

            let input = generate_sine_wave_i16(440.0, 44100, 1024);

            let output = resampler.process(&input).unwrap();

            // Output should be approximately 48000/44100 * 1024 = ~1115 frames
            let output_frames = output.len() / 2;
            assert!(
                output_frames >= 1050 && output_frames <= 1200,
                "Expected ~1115 frames, got {}",
                output_frames
            );
        }

        #[test]
        fn handles_empty_input() {
            let mut resampler = Resampler::new(48000, 44100, 2).unwrap();
            let output = resampler.process(&[]).unwrap();
            assert!(output.is_empty());
        }
    }

    mod f32_processing {
        use super::*;

        #[test]
        fn resamples_f32_correctly() {
            let mut resampler = Resampler::new(48000, 44100, 2).unwrap();

            let input = generate_sine_wave_f32(440.0, 48000, 1024);

            let output = resampler.process_f32(&input).unwrap();

            assert!(!output.is_empty());
            assert!(output[0].len() >= 900 && output[0].len() <= 1000);

            // Verify audio is not silent
            let rms = calculate_rms_f32(&output[0]);
            assert!(rms > 0.3, "Output RMS too low: {}", rms);
        }
    }

    mod priming {
        use super::*;

        #[test]
        fn primed_resampler_has_consistent_amplitude() {
            let mut resampler = Resampler::new(48000, 44100, 2).unwrap();

            // Generate and process audio
            let mut all_output: Vec<Vec<f32>> = vec![Vec::new(), Vec::new()];

            for _ in 0..20 {
                let input = generate_sine_wave_f32(440.0, 48000, 1024);
                let output = resampler.process_f32(&input).unwrap();
                if !output.is_empty() {
                    all_output[0].extend(&output[0]);
                    all_output[1].extend(&output[1]);
                }
            }

            // First 1000 samples should have similar RMS to middle
            let first_rms = calculate_rms_f32(&all_output[0][..1000.min(all_output[0].len())]);
            let mid_start = all_output[0].len() / 2;
            let mid_rms = calculate_rms_f32(&all_output[0][mid_start..mid_start + 1000]);

            let ratio = first_rms / mid_rms;
            assert!(
                ratio > 0.8,
                "First samples should have similar RMS to middle, ratio = {}",
                ratio
            );
        }
    }

    mod dithering {
        use super::*;

        #[test]
        fn dither_to_i16_preserves_range() {
            // Test various input values
            assert!(dither_to_i16(0.0).abs() < 100); // Near zero with noise
            assert!(dither_to_i16(1.0) > 30000); // Near max
            assert!(dither_to_i16(-1.0) < -30000); // Near min
        }

        #[test]
        fn interleave_with_dither_correct_length() {
            let channels = vec![vec![0.5f32; 100], vec![-0.5f32; 100]];
            let output = interleave_with_dither(&channels);
            assert_eq!(output.len(), 200); // 100 frames * 2 channels
        }

        #[test]
        fn interleave_with_dither_handles_empty() {
            let channels: Vec<Vec<f32>> = vec![];
            let output = interleave_with_dither(&channels);
            assert!(output.is_empty());
        }
    }

    mod reset {
        use super::*;

        #[test]
        fn reset_clears_state() {
            let mut resampler = Resampler::new(48000, 44100, 2).unwrap();

            // Process some audio
            let input = generate_sine_wave_i16(440.0, 48000, 1024);
            let _ = resampler.process(&input);

            // Reset
            resampler.reset();

            // Process again - should work fine
            let output = resampler.process(&input).unwrap();
            assert!(!output.is_empty());
        }
    }
}
