/// Percentile par rang le plus proche (nearest-rank). `sorted` doit être trié croissant.
pub fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
    Some(sorted[rank.min(sorted.len()) - 1])
}

pub fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank() {
        let v: Vec<f64> = (1..=20).map(|x| x as f64).collect();
        assert_eq!(percentile(&v, 50.0), Some(10.0));
        assert_eq!(percentile(&v, 95.0), Some(19.0));
        assert_eq!(percentile(&v, 100.0), Some(20.0));
        assert_eq!(percentile(&[], 50.0), None);
        assert_eq!(percentile(&[7.0], 95.0), Some(7.0));
    }

    #[test]
    fn rounds_to_tenth() {
        assert_eq!(round1(12.345), 12.3);
        assert_eq!(round1(12.35), 12.4);
    }
}
