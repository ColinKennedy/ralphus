//! Statistics bundle computed from a durable-minimum run's raw samples
//! (RAL-94). See `cli/src/ralphus/bench/stats.py` for the Python analog.
//! The interpolation method need not match bit-for-bit between the two —
//! Rust and Python bench data are stored, graphed, and inspected
//! independently, never compared to each other numerically.

#[derive(Debug, Clone)]
pub struct StatsBundle {
    pub durable_min: f64,
    pub max: f64,
    pub mean: f64,
    pub median: f64,
    pub stddev: f64,
    pub iqr: f64,
    pub outliers: Vec<f64>,
    pub samples: Vec<f64>,
}

/// Computes the full stats bundle from raw samples. Outliers are flagged via
/// Tukey's method: values outside 1.5*IQR from Q1/Q3. Requires at least two
/// samples (guaranteed by `run_durable_min`, since `patience >= 1` always
/// yields at least two invocations).
pub fn compute_stats(samples: &[f64], durable_min: f64) -> StatsBundle {
    assert!(
        samples.len() >= 2,
        "compute_stats requires >= 2 samples, got {}",
        samples.len()
    );

    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("durations are never NaN"));

    let max = sorted[sorted.len() - 1];
    let mean = sorted.iter().sum::<f64>() / sorted.len() as f64;
    let median = quantile(&sorted, 0.5);
    let variance =
        sorted.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (sorted.len() - 1) as f64;
    let stddev = variance.sqrt();

    let q1 = quantile(&sorted, 0.25);
    let q3 = quantile(&sorted, 0.75);
    let iqr = q3 - q1;
    let lower_fence = q1 - 1.5 * iqr;
    let upper_fence = q3 + 1.5 * iqr;
    let outliers: Vec<f64> = sorted
        .iter()
        .copied()
        .filter(|&d| d < lower_fence || d > upper_fence)
        .collect();

    StatsBundle {
        durable_min,
        max,
        mean,
        median,
        stddev,
        iqr,
        outliers,
        samples: samples.to_vec(),
    }
}

/// Linear-interpolation quantile (`0.0..=1.0`) over an already-sorted slice.
fn quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.len() == 1 {
        return sorted[0];
    }
    let pos = q * (sorted.len() - 1) as f64;
    let lower = pos.floor() as usize;
    let upper = pos.ceil() as usize;
    if lower == upper {
        return sorted[lower];
    }
    let frac = pos - lower as f64;
    sorted[lower] + (sorted[upper] - sorted[lower]) * frac
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_bundle_matches_hand_computed_values() {
        let samples = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let bundle = compute_stats(&samples, 1.0);
        assert_eq!(bundle.durable_min, 1.0);
        assert_eq!(bundle.max, 5.0);
        assert_eq!(bundle.mean, 3.0);
        assert_eq!(bundle.median, 3.0);
        assert!((bundle.stddev - 1.5811388300841898).abs() < 1e-9);
        assert_eq!(bundle.iqr, 2.0);
        assert!(bundle.outliers.is_empty());
    }

    #[test]
    fn flags_tukey_outliers() {
        let samples = vec![1.0, 2.0, 2.0, 2.0, 2.0, 2.0, 100.0];
        let bundle = compute_stats(&samples, 1.0);
        assert!(bundle.outliers.contains(&100.0));
    }

    #[test]
    #[should_panic(expected = "compute_stats requires >= 2 samples")]
    fn rejects_single_sample() {
        compute_stats(&[1.0], 1.0);
    }
}
