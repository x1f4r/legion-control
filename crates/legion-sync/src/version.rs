//! Version vectors for causal ordering of per-path state across sync members.
//!
//! A [`VersionVector`] maps each member identifier to a strictly positive monotonic counter.
//! Zero entries are never stored so two vectors with identical semantics always compare equal.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::model::MemberId;

/// Causal relationship between two [`VersionVector`] values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VersionOrder {
    /// Both vectors have identical counters for every member.
    Equal,
    /// `self` strictly dominates `other` (every counter `>=`, at least one `>`).
    Newer,
    /// `other` strictly dominates `self` (every counter `<=`, at least one `<`).
    Older,
    /// Neither dominates the other (`self` is ahead on at least one member and `other` is ahead
    /// on at least one other member).
    Concurrent,
}

/// Per-member monotonic counter map tracking the causal history of a single path.
///
/// Missing entries are implicitly `0`, and [`VersionVector::set`] removes any entry set to `0` so
/// the representation is always canonical.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct VersionVector {
    counters: BTreeMap<MemberId, u64>,
}

impl VersionVector {
    /// Creates an empty version vector (all member counters implicitly `0`).
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a version vector with a single `(member, counter)` pair.
    pub fn singleton(member: impl Into<MemberId>, counter: u64) -> Self {
        let mut vv = Self::new();
        vv.set(member, counter);
        vv
    }

    /// Constructs a canonical version vector from a raw map, dropping any zero entries.
    pub fn from_map(map: BTreeMap<MemberId, u64>) -> Self {
        let counters = map.into_iter().filter(|(_, v)| *v > 0).collect();
        Self { counters }
    }

    /// Returns the counter for `member` (`0` if absent).
    pub fn get(&self, member: &str) -> u64 {
        self.counters.get(member).copied().unwrap_or(0)
    }

    /// Sets `member`'s counter to `value`, removing the entry if `value == 0`.
    pub fn set(&mut self, member: impl Into<MemberId>, value: u64) {
        let member = member.into();
        if value == 0 {
            self.counters.remove(&member);
        } else {
            self.counters.insert(member, value);
        }
    }

    /// Raises `member`'s counter to at least `value` without ever decreasing it.
    pub fn raise(&mut self, member: impl Into<MemberId>, value: u64) {
        if value == 0 {
            return;
        }
        let member = member.into();
        let entry = self.counters.entry(member).or_insert(0);
        if value > *entry {
            *entry = value;
        }
    }

    /// Advances `member`'s counter to strictly exceed both its current value and `floor`.
    ///
    /// Returns the new counter value.
    pub fn bump_above(&mut self, member: impl Into<MemberId>, floor: u64) -> u64 {
        let member = member.into();
        let current = self.counters.get(&member).copied().unwrap_or(0);
        let next = current.max(floor).saturating_add(1);
        self.counters.insert(member, next);
        next
    }

    /// Increments `member`'s counter by 1 and returns the new value.
    pub fn increment(&mut self, member: impl Into<MemberId>) -> u64 {
        let member = member.into();
        let next = self
            .counters
            .get(&member)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        self.counters.insert(member, next);
        next
    }

    /// Returns `true` if all counters are zero.
    pub fn is_empty(&self) -> bool {
        self.counters.is_empty()
    }

    /// Returns the number of members with non-zero counters.
    pub fn len(&self) -> usize {
        self.counters.len()
    }

    /// Iterates over `(member, counter)` pairs in deterministic member-name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, u64)> {
        self.counters.iter().map(|(k, v)| (k.as_str(), *v))
    }

    /// Borrows the underlying canonical counter map.
    pub fn as_map(&self) -> &BTreeMap<MemberId, u64> {
        &self.counters
    }

    /// Compares `self` causally against `other`.
    pub fn compare(&self, other: &Self) -> VersionOrder {
        let mut self_greater = false;
        let mut other_greater = false;

        let mut it_a = self.counters.iter().peekable();
        let mut it_b = other.counters.iter().peekable();

        while let (Some((ka, va)), Some((kb, vb))) = (it_a.peek(), it_b.peek()) {
            match ka.cmp(kb) {
                Ordering::Less => {
                    // Present in `self` (> 0) and absent in `other` (0).
                    self_greater = true;
                    it_a.next();
                }
                Ordering::Greater => {
                    // Absent in `self` (0) and present in `other` (> 0).
                    other_greater = true;
                    it_b.next();
                }
                Ordering::Equal => {
                    match va.cmp(vb) {
                        Ordering::Greater => self_greater = true,
                        Ordering::Less => other_greater = true,
                        Ordering::Equal => {}
                    }
                    it_a.next();
                    it_b.next();
                }
            }
            if self_greater && other_greater {
                return VersionOrder::Concurrent;
            }
        }

        if it_a.peek().is_some() {
            self_greater = true;
        }
        if it_b.peek().is_some() {
            other_greater = true;
        }

        match (self_greater, other_greater) {
            (false, false) => VersionOrder::Equal,
            (true, false) => VersionOrder::Newer,
            (false, true) => VersionOrder::Older,
            (true, true) => VersionOrder::Concurrent,
        }
    }

    /// Returns `true` when `self` causally dominates or equals `other`.
    pub fn dominates_or_equal(&self, other: &Self) -> bool {
        matches!(
            self.compare(other),
            VersionOrder::Equal | VersionOrder::Newer
        )
    }

    /// Returns the least upper bound (componentwise maximum) of `self` and `other`.
    pub fn merge(&self, other: &Self) -> Self {
        let mut out = self.clone();
        out.merge_in_place(other);
        out
    }

    /// Merges `other` into `self` in place via componentwise maximum.
    pub fn merge_in_place(&mut self, other: &Self) {
        for (member, &val) in &other.counters {
            if val == 0 {
                continue;
            }
            let entry = self.counters.entry(member.clone()).or_insert(0);
            if val > *entry {
                *entry = val;
            }
        }
    }
}

impl fmt::Display for VersionVector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("{")?;
        for (i, (member, val)) in self.counters.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{member}:{val}")?;
        }
        f.write_str("}")
    }
}

impl Serialize for VersionVector {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.counters.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for VersionVector {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = BTreeMap::<MemberId, u64>::deserialize(deserializer)?;
        Ok(Self::from_map(raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_representation_drops_zeros() {
        let mut vv = VersionVector::new();
        vv.set("tower", 0);
        assert!(vv.is_empty());
        vv.set("tower", 3);
        assert_eq!(vv.get("tower"), 3);
        vv.set("tower", 0);
        assert!(vv.is_empty());
        assert_eq!(vv.get("tower"), 0);

        let from_json: VersionVector = serde_json::from_str(r#"{"tower":0,"pi":2}"#).unwrap();
        assert_eq!(from_json, VersionVector::singleton("pi", 2));
    }

    #[test]
    fn compare_and_merge_basics() {
        let a = VersionVector::singleton("tower", 2);
        let b = VersionVector::singleton("tower", 1);
        assert_eq!(a.compare(&b), VersionOrder::Newer);
        assert_eq!(b.compare(&a), VersionOrder::Older);
        assert_eq!(a.compare(&a), VersionOrder::Equal);

        let c = VersionVector::singleton("pi", 1);
        assert_eq!(a.compare(&c), VersionOrder::Concurrent);
        assert_eq!(c.compare(&a), VersionOrder::Concurrent);

        let merged = a.merge(&c);
        assert_eq!(merged.get("tower"), 2);
        assert_eq!(merged.get("pi"), 1);
        assert_eq!(merged.compare(&a), VersionOrder::Newer);
        assert_eq!(merged.compare(&c), VersionOrder::Newer);
    }
}
