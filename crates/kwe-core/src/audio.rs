// SPDX-License-Identifier: Apache-2.0
//! Bounded, dependency-free audio analysis used by a future PipeWire worker.

use std::f32::consts::PI;

use kwe_input_protocol::AudioFrame;

const MAX_SAMPLES: usize = 8192;

pub fn analyze_stereo(
    sequence: u64,
    left: &[f32],
    right: &[f32],
    bands: usize,
) -> Result<AudioFrame, String> {
    if left.len() != right.len() || left.is_empty() || left.len() > MAX_SAMPLES {
        return Err(
            "audio window must have matching non-empty channels within the safety limit".into(),
        );
    }
    if !matches!(bands, 16 | 32 | 64) {
        return Err("bands must be 16, 32, or 64".into());
    }
    let l = spectrum(left, bands);
    let r = spectrum(right, bands);
    AudioFrame::new(sequence, l, r).map_err(|error| error.to_string())
}

fn spectrum(samples: &[f32], bands: usize) -> Vec<f32> {
    let n = samples.len() as f32;
    (0..bands)
        .map(|band| {
            let start = band * samples.len() / bands;
            let end = ((band + 1) * samples.len() / bands)
                .max(start + 1)
                .min(samples.len());
            let energy = samples[start..end]
                .iter()
                .enumerate()
                .map(|(offset, sample)| {
                    let window = 0.5
                        - 0.5
                            * (2.0 * PI * offset as f32 / end.saturating_sub(start).max(1) as f32)
                                .cos();
                    (sample * window).abs()
                })
                .sum::<f32>()
                / n.max(1.0);
            energy.clamp(0.0, 1.0)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_bounded_stereo_bands() {
        let samples = vec![0.25; 256];
        let frame = analyze_stereo(1, &samples, &samples, 32).unwrap();
        assert_eq!(frame.left.len(), 32);
        assert!(frame.left.iter().all(|value| (0.0..=1.0).contains(value)));
    }

    #[test]
    fn rejects_unbounded_windows_and_band_counts() {
        let samples = vec![0.0; MAX_SAMPLES + 1];
        assert!(analyze_stereo(1, &samples, &samples, 16).is_err());
        assert!(analyze_stereo(1, &[0.0; 8], &[0.0; 8], 8).is_err());
    }
}
