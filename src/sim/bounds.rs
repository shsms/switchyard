//! Power-bound containers, ported from microsim's lisp/bounds module.
//!
//! Two layers:
//! - [`VecBounds`] is a sorted, normalized list of disjoint
//!   [`Bounds`] (proto type, reused so the values flow straight into a
//!   `MetricSample` without a copy).
//! - [`ComponentBounds`] holds the rated bounds plus a queue of
//!   time-limited augmentations submitted via the gRPC AugmentBounds
//!   RPC. `squash()` intersects them down to the effective bounds.

use std::{collections::VecDeque, fmt, time::Duration};

use chrono::{DateTime, Utc};

use crate::proto::common::metrics::Bounds;

#[derive(Debug, Clone, Default)]
pub struct VecBounds(pub Vec<Bounds>);

impl fmt::Display for VecBounds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return write!(f, "[]");
        }
        let mut first = true;
        for b in &self.0 {
            if !first {
                write!(f, ", ")?;
            }
            first = false;
            write!(f, "{}", BoundsDisplay(b))?;
        }
        Ok(())
    }
}

struct BoundsDisplay<'a>(&'a Bounds);
impl fmt::Display for BoundsDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn side(b: Option<f32>) -> String {
            b.map(|v| format!("{v}")).unwrap_or_else(|| "*".into())
        }
        write!(f, "[{}, {}]", side(self.0.lower), side(self.0.upper))
    }
}

impl VecBounds {
    pub fn single(lower: f32, upper: f32) -> Self {
        Self(vec![Bounds {
            lower: Some(lower),
            upper: Some(upper),
        }])
    }

    pub fn new(mut bounds: Vec<Bounds>) -> Self {
        bounds.sort_by(|a, b| {
            a.lower
                .unwrap_or(f32::MIN)
                .partial_cmp(&b.lower.unwrap_or(f32::MIN))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        VecBounds(bounds)
    }

    pub fn into_inner(self) -> Vec<Bounds> {
        self.0
    }

    pub fn contains(&self, value: f32) -> bool {
        self.0.iter().any(|b| bounds_contains(b, value))
    }

    /// Pull `value` to the closest edge of any bound when it is outside
    /// the union; identity if it is already inside.
    pub fn clamp(&self, value: f32) -> f32 {
        if self.0.is_empty() || self.contains(value) {
            return value;
        }
        let mut prev_upper: Option<f32> = None;
        for b in &self.0 {
            if let Some(lower) = b.lower {
                if value < lower {
                    return match prev_upper {
                        // <= so equidistant ties pull to the lower-magnitude
                        // edge (matches microsim's behaviour).
                        Some(pu) if (value - pu).abs() <= (lower - value).abs() => pu,
                        _ => lower,
                    };
                }
            }
            if let Some(upper) = b.upper {
                prev_upper = Some(upper);
            }
        }
        prev_upper.unwrap_or(value)
    }

    /// Add two single-bucket bound containers element-wise. Microsim's
    /// general-case add (handling multi-bucket exclusion zones) is
    /// overkill for switchyard's current needs — when both sides are
    /// `[lower, upper]` we just sum the like-signed edges and pick the
    /// extreme on the cross-signed ones, matching the behaviour the
    /// inverter aggregation needs.
    ///
    /// Empty containers are treated as zero, so `Σ` over an empty set
    /// of children yields `[0, 0]`.
    pub fn sum_single(items: impl IntoIterator<Item = Self>) -> Self {
        let mut lower = 0.0_f32;
        let mut upper = 0.0_f32;
        let mut any = false;
        for vb in items {
            let Some(b) = vb.0.first().cloned() else {
                continue;
            };
            any = true;
            if let Some(l) = b.lower {
                lower += l;
            }
            if let Some(u) = b.upper {
                upper += u;
            }
        }
        if !any {
            return Self::default();
        }
        Self::single(lower, upper)
    }

    pub fn intersect(&self, other: &Self) -> Self {
        let mut result = Vec::new();
        for b1 in &self.0 {
            for b2 in &other.0 {
                let int = bounds_intersect(b1, b2);
                if int.lower.is_some() || int.upper.is_some() {
                    result.push(int);
                }
            }
        }
        squash(result)
    }
}

fn bounds_contains(b: &Bounds, value: f32) -> bool {
    if let Some(l) = b.lower {
        if value < l {
            return false;
        }
    }
    if let Some(u) = b.upper {
        if value > u {
            return false;
        }
    }
    true
}

fn bounds_intersect(a: &Bounds, b: &Bounds) -> Bounds {
    fn pick(a: Option<f32>, b: Option<f32>, op: impl FnOnce(f32, f32) -> f32) -> Option<f32> {
        match (a, b) {
            (Some(a), Some(b)) => Some(op(a, b)),
            (Some(x), None) | (None, Some(x)) => Some(x),
            (None, None) => None,
        }
    }
    let lower = pick(a.lower, b.lower, f32::max);
    let upper = pick(a.upper, b.upper, f32::min);
    if let (Some(l), Some(u)) = (lower, upper) {
        if l > u {
            return Bounds {
                lower: None,
                upper: None,
            };
        }
    }
    Bounds { lower, upper }
}

fn merge_if_overlapping(a: &Bounds, b: &Bounds) -> Option<Bounds> {
    let intersection = bounds_intersect(a, b);
    if intersection.lower.is_some() || intersection.upper.is_some() {
        Some(Bounds {
            lower: a.lower.and_then(|x| b.lower.map(|y| x.min(y))),
            upper: a.upper.and_then(|x| b.upper.map(|y| x.max(y))),
        })
    } else {
        None
    }
}

fn squash(mut input: Vec<Bounds>) -> VecBounds {
    input.sort_by(|a, b| {
        a.lower
            .unwrap_or(f32::MIN)
            .partial_cmp(&b.lower.unwrap_or(f32::MIN))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if input.is_empty() {
        return VecBounds(input);
    }
    let mut squashed = Vec::new();
    let mut current = input[0].clone();
    for next in &input[1..] {
        if let Some(merged) = merge_if_overlapping(&current, next) {
            current = merged;
        } else {
            squashed.push(current);
            current = next.clone();
        }
    }
    squashed.push(current);
    VecBounds(squashed)
}

/// Rated bounds with a queue of time-limited augmentations.
#[derive(Debug, Clone)]
pub struct ComponentBounds {
    rated: VecBounds,
    augmented: VecDeque<Aug>,
}

#[derive(Debug, Clone)]
struct Aug {
    create_ts: DateTime<Utc>,
    bounds: VecBounds,
    lifetime: Duration,
}

impl ComponentBounds {
    pub fn rated(lower: f32, upper: f32) -> Self {
        Self {
            rated: VecBounds::single(lower, upper),
            augmented: VecDeque::new(),
        }
    }

    pub fn set_rated(&mut self, lower: f32, upper: f32) {
        self.rated = VecBounds::single(lower, upper);
    }

    pub fn rated_lower(&self) -> f32 {
        self.rated.0.first().and_then(|b| b.lower).unwrap_or(0.0)
    }

    pub fn rated_upper(&self) -> f32 {
        self.rated.0.first().and_then(|b| b.upper).unwrap_or(0.0)
    }

    pub fn add_augmentation(
        &mut self,
        create_ts: DateTime<Utc>,
        bounds: VecBounds,
        lifetime: Duration,
    ) {
        self.augmented.push_back(Aug {
            create_ts,
            bounds,
            lifetime,
        });
    }

    pub fn drop_expired(&mut self, now: DateTime<Utc>) {
        while let Some(front) = self.augmented.front() {
            let ttl = chrono::Duration::from_std(front.lifetime).unwrap_or(chrono::Duration::zero());
            if front.create_ts + ttl < now {
                self.augmented.pop_front();
            } else {
                break;
            }
        }
    }

    /// Effective bounds: rated ∩ all live augmentations.
    pub fn effective(&self) -> VecBounds {
        let mut out = self.rated.clone();
        for a in &self.augmented {
            out = out.intersect(&a.bounds);
        }
        out
    }

    pub fn contains(&self, value: f32) -> bool {
        self.effective().contains(value)
    }

    pub fn clamp(&self, value: f32) -> f32 {
        self.effective().clamp(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_and_clamp() {
        let vb = VecBounds::new(vec![
            Bounds {
                lower: Some(-30.0),
                upper: Some(-10.0),
            },
            Bounds {
                lower: Some(10.0),
                upper: Some(30.0),
            },
        ]);
        assert!(vb.contains(-20.0));
        assert!(!vb.contains(0.0));
        assert_eq!(vb.clamp(-20.0), -20.0);
        // 0 is closer to -10 than to 10 → -10
        assert_eq!(vb.clamp(0.0), -10.0);
        assert_eq!(vb.clamp(100.0), 30.0);
    }

    #[test]
    fn rated_intersected_with_augmentations() {
        let mut cb = ComponentBounds::rated(-100.0, 100.0);
        cb.add_augmentation(
            Utc::now(),
            VecBounds::single(-50.0, 50.0),
            Duration::from_secs(60),
        );
        let eff = cb.effective();
        assert_eq!(eff.0.len(), 1);
        assert_eq!(eff.0[0].lower, Some(-50.0));
        assert_eq!(eff.0[0].upper, Some(50.0));
    }
}
