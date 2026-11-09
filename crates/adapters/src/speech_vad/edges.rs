use aivtuber_domain::SpeechEdge;

/// Start/end probability thresholds. These are the Silero hysteresis values
/// EXP-0001 measured (0.5 / 0.35). They are not a debounce: a single frame
/// above the start threshold is an edge, and a single frame below the end
/// threshold ends it (ADR-0005).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HysteresisConfig {
    pub start_threshold: f32,
    pub end_threshold: f32,
}

impl Default for HysteresisConfig {
    fn default() -> Self {
        Self {
            start_threshold: 0.5,
            end_threshold: 0.35,
        }
    }
}

impl HysteresisConfig {
    pub fn validate(&self) -> Result<(), String> {
        let ok = self.end_threshold > 0.0
            && self.end_threshold <= self.start_threshold
            && self.start_threshold <= 1.0;
        if ok {
            Ok(())
        } else {
            Err("thresholds must satisfy 0 < end <= start <= 1".to_owned())
        }
    }
}

/// Per-frame probability to raw activity edges.
#[derive(Debug, Clone)]
pub struct EdgeDetector {
    config: HysteresisConfig,
    active: bool,
}

impl EdgeDetector {
    pub fn new(config: HysteresisConfig) -> Result<Self, String> {
        config.validate()?;
        Ok(Self {
            config,
            active: false,
        })
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Feed one frame probability. A non-finite probability carries no
    /// information and changes nothing.
    pub fn push(&mut self, probability: f32) -> Option<SpeechEdge> {
        if !probability.is_finite() {
            return None;
        }
        if !self.active && probability >= self.config.start_threshold {
            self.active = true;
            Some(SpeechEdge::Start)
        } else if self.active && probability < self.config.end_threshold {
            self.active = false;
            Some(SpeechEdge::End)
        } else {
            None
        }
    }

    /// Close an open segment after a capture gap or at end of input, so the
    /// consumer is never left believing the speaker is still talking.
    pub fn force_end(&mut self) -> bool {
        std::mem::replace(&mut self.active, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detector() -> EdgeDetector {
        EdgeDetector::new(HysteresisConfig::default()).unwrap()
    }

    #[test]
    fn silence_produces_no_edges() {
        let mut d = detector();
        assert!((0..1000).all(|_| d.push(0.02).is_none()));
    }

    #[test]
    fn a_single_frame_blip_is_a_start_and_an_end_with_no_debounce() {
        let mut d = detector();
        assert_eq!(d.push(0.9), Some(SpeechEdge::Start));
        assert_eq!(d.push(0.1), Some(SpeechEdge::End));
    }

    #[test]
    fn hysteresis_holds_between_the_thresholds() {
        let mut d = detector();
        assert_eq!(d.push(0.6), Some(SpeechEdge::Start));
        assert_eq!(d.push(0.4), None);
        assert_eq!(d.push(0.36), None);
        assert_eq!(d.push(0.34), Some(SpeechEdge::End));
        assert_eq!(d.push(0.45), None);
    }

    #[test]
    fn non_finite_probabilities_change_nothing() {
        let mut d = detector();
        assert_eq!(d.push(f32::NAN), None);
        d.push(0.9);
        assert_eq!(d.push(f32::NAN), None);
        assert!(d.is_active());
    }

    #[test]
    fn force_end_closes_once() {
        let mut d = detector();
        d.push(0.9);
        assert!(d.force_end());
        assert!(!d.force_end());
    }

    #[test]
    fn invalid_thresholds_are_rejected() {
        let bad = HysteresisConfig {
            start_threshold: 0.3,
            end_threshold: 0.5,
        };
        assert!(EdgeDetector::new(bad).is_err());
    }
}
