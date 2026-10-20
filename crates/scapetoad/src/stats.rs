//! Cartogram quality statistics.

/// Size error of each feature, in percent: 100 means the feature got exactly its share of
/// the total area; 200 means it is twice too small, 50 twice too large.
///
/// `err = 100 · (mass share) / (area share)`, ScapeToad's formula, where the mass is the
/// population value (or density × original area when the attribute is a density).
pub fn size_errors(masses: &[f64], areas: &[f64]) -> Vec<f64> {
    let total_mass: f64 = masses.iter().sum();
    let total_area: f64 = areas.iter().sum();
    masses
        .iter()
        .zip(areas)
        .map(|(&m, &a)| {
            if a > 0.0 && total_mass > 0.0 && total_area > 0.0 {
                100.0 * (m / total_mass) / (a / total_area)
            } else {
                0.0
            }
        })
        .collect()
}

/// Summary of the size errors over all features.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ErrorSummary {
    pub count: usize,
    pub mean: f64,
    pub std_dev: f64,
    pub p25: f64,
    pub p50: f64,
    pub p75: f64,
    /// Number of features whose error is within one standard deviation of the mean.
    pub within_one_std_dev: usize,
}

impl ErrorSummary {
    pub fn from_values(values: &[f64]) -> Self {
        let count = values.len();
        if count == 0 {
            return Self {
                count,
                mean: 0.0,
                std_dev: 0.0,
                p25: 0.0,
                p50: 0.0,
                p75: 0.0,
                within_one_std_dev: 0,
            };
        }
        let n = count as f64;
        let mean = values.iter().sum::<f64>() / n;
        let std_dev = (values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n).sqrt();
        let mut sorted = values.to_vec();
        sorted.sort_unstable_by(f64::total_cmp);
        let within = values
            .iter()
            .filter(|&&v| v >= mean - std_dev && v <= mean + std_dev)
            .count();
        Self {
            count,
            mean,
            std_dev,
            p25: percentile(&sorted, 25.0),
            p50: percentile(&sorted, 50.0),
            p75: percentile(&sorted, 75.0),
            within_one_std_dev: within,
        }
    }
}

/// Linear-interpolation percentile of sorted values (`p` in 0..=100).
///
/// ScapeToad collected values in a `TreeSet`, which dropped duplicates and could index past
/// the end; this keeps every value.
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    match sorted.len() {
        0 => 0.0,
        1 => sorted[0],
        n => {
            let rank = p.clamp(0.0, 100.0) / 100.0 * (n - 1) as f64;
            let lo = rank.floor() as usize;
            let hi = rank.ceil() as usize;
            sorted[lo] + (rank - lo as f64) * (sorted[hi] - sorted[lo])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perfect_cartogram_has_error_100() {
        let errs = size_errors(&[10.0, 30.0], &[1.0, 3.0]);
        assert_eq!(errs, vec![100.0, 100.0]);
    }

    #[test]
    fn percentiles_keep_duplicates() {
        let sorted = [1.0, 1.0, 1.0, 5.0];
        assert_eq!(percentile(&sorted, 50.0), 1.0);
        assert_eq!(percentile(&sorted, 100.0), 5.0);
    }
}
