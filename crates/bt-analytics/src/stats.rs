//! Statistical primitives (f64 domain, documented boundary). Implementations
//! are small, explicit, and reference-tested — no hidden library behavior.

/// Arithmetic mean; None for empty input.
pub fn mean(xs: &[f64]) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    Some(xs.iter().sum::<f64>() / xs.len() as f64)
}

/// Sample standard deviation (n-1). Returns Some(0.0) for constant series.
pub fn sample_std(xs: &[f64]) -> Option<f64> {
    let n = xs.len();
    if n < 2 {
        return None;
    }
    let m = mean(xs)?;
    let var = xs.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (n - 1) as f64;
    Some(var.sqrt())
}

/// Population standard deviation (n).
pub fn population_std(xs: &[f64]) -> Option<f64> {
    let n = xs.len();
    if n == 0 {
        return None;
    }
    let m = mean(xs)?;
    let var = xs.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / n as f64;
    Some(var.sqrt())
}

/// Downside deviation vs a minimum acceptable return (MAR).
pub fn downside_dev(xs: &[f64], mar: f64) -> Option<f64> {
    let n = xs.len();
    if n == 0 {
        return None;
    }
    let var = xs
        .iter()
        .map(|r| {
            let d = (r - mar).min(0.0);
            d * d
        })
        .sum::<f64>()
        / n as f64;
    Some(var.sqrt())
}

/// Sample skewness (g1). None if variance is zero or n < 3.
pub fn skewness(xs: &[f64]) -> Option<f64> {
    let n = xs.len();
    if n < 3 {
        return None;
    }
    let m = mean(xs)?;
    let s = sample_std(xs)?;
    if s == 0.0 {
        return None;
    }
    let g1 = xs.iter().map(|x| ((x - m) / s).powi(3)).sum::<f64>() * n as f64
        / ((n - 1) as f64 * (n - 2) as f64);
    Some(g1)
}

/// Sample excess kurtosis (g2). None if variance is zero or n < 4.
pub fn kurtosis_excess(xs: &[f64]) -> Option<f64> {
    let n = xs.len();
    if n < 4 {
        return None;
    }
    let m = mean(xs)?;
    let s = sample_std(xs)?;
    if s == 0.0 {
        return None;
    }
    let sum4 = xs.iter().map(|x| ((x - m) / s).powi(4)).sum::<f64>();
    let n = n as f64;
    let g2 = sum4 * (n + 1.0) / ((n - 1.0) * (n - 2.0) * (n - 3.0))
        - 3.0 * (n - 1.0) * (n - 1.0) / ((n - 2.0) * (n - 3.0));
    Some(g2)
}

/// Median (average of the two middle values for even n).
pub fn median(xs: &[f64]) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    })
}

/// Linear interpolation percentile on [0, 100].
pub fn percentile(xs: &[f64], p: f64) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n == 1 {
        return Some(v[0]);
    }
    let rank = (p / 100.0) * (n - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    let frac = rank - lo as f64;
    Some(v[lo] * (1.0 - frac) + v[hi] * frac)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mean_and_std_reference() {
        let xs = [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
        let m = mean(&xs).unwrap();
        assert!((m - 5.0).abs() < 1e-12);
        let s = sample_std(&xs).unwrap();
        assert!((s - 2.138089935299395).abs() < 1e-12);
        let p = population_std(&xs).unwrap();
        assert!((p - 2.0).abs() < 1e-12);
    }

    #[test]
    fn median_and_percentiles() {
        assert_eq!(median(&[3.0, 1.0, 2.0]).unwrap(), 2.0);
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]).unwrap(), 2.5);
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0, 5.0], 50.0).unwrap(), 3.0);
        assert!((percentile(&[1.0, 2.0, 3.0, 4.0, 5.0], 95.0).unwrap() - 4.8).abs() < 1e-12);
    }

    #[test]
    fn skew_kurtosis_reference() {
        // Uniform-ish symmetric series => skew ~ 0
        let xs: Vec<f64> = vec![-2.0, -1.0, -1.0, 0.0, 0.0, 1.0, 1.0, 2.0];
        let sk = skewness(&xs).unwrap();
        assert!(sk.abs() < 1e-12);
        // Constant series => None (documented)
        assert!(skewness(&[1.0, 1.0, 1.0]).is_none());
    }

    #[test]
    fn downside_dev_only_negative() {
        let xs = [0.05, -0.02, 0.03, -0.01];
        let dd = downside_dev(&xs, 0.0).unwrap();
        // sqrt((0.02^2 + 0.01^2)/4)
        let expected = ((0.0004 + 0.0001) / 4.0f64).sqrt();
        assert!((dd - expected).abs() < 1e-15);
    }
}
