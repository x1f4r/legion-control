//! Version vectors for causal ordering of per-path state across sync member incarnations.
//!
//! Each entry is keyed by `(member, incarnation)` — a [`VersionKey`] — so restoring a machine
//! from backup or reinstalling its index starts a fresh incarnation and never collides with or
//! repeats counters from an earlier incarnation. Zero counters are never stored so two vectors
//! with identical semantics always compare equal.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::model::MemberId;

/// Causal key identifying one incarnation of a folder member.
///
/// Serialized in JSON maps as `"<member>#<incarnation_hex>"` so every member incarnation occupies
/// a distinct, collision-free slot in [`VersionVector`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VersionKey {
    /// Member (machine) identifier.
    pub member: MemberId,
    /// Random non-zero incarnation identifier for this member's index.
    pub incarnation: u64,
}

impl VersionKey {
    /// Constructs a new `(member, incarnation)` key.
    pub fn new(member: impl Into<MemberId>, incarnation: u64) -> Self {
        Self {
            member: member.into(),
            incarnation,
        }
    }

    /// Formats this key as `"<member>#<incarnation_hex>"`.
    pub fn to_wire_string(&self) -> String {
        format!("{}#{:016x}", self.member, self.incarnation)
    }

    /// Parses `"<member>#<incarnation_hex>"` back into a [`VersionKey`].
    pub fn parse_wire_string(raw: &str) -> std::result::Result<Self, String> {
        let Some((member, hex)) = raw.rsplit_once('#') else {
            return Err(format!(
                "version key {raw:?} missing '#' incarnation suffix"
            ));
        };
        if member.is_empty() || hex.is_empty() || hex.len() > 16 {
            return Err(format!("malformed version key {raw:?}"));
        }
        let incarnation = u64::from_str_radix(hex, 16)
            .map_err(|_| format!("invalid incarnation hex in version key {raw:?}"))?;
        if incarnation == 0 {
            return Err(format!(
                "incarnation must be non-zero in version key {raw:?}"
            ));
        }
        Ok(Self {
            member: member.to_string(),
            incarnation,
        })
    }
}

impl fmt::Display for VersionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{:016x}", self.member, self.incarnation)
    }
}

impl Serialize for VersionKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_wire_string())
    }
}

impl<'de> Deserialize<'de> for VersionKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse_wire_string(&raw).map_err(de::Error::custom)
    }
}

/// Causal relationship between two [`VersionVector`] values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VersionOrder {
    /// Both vectors have identical counters for every `(member, incarnation)` key.
    Equal,
    /// `self` strictly dominates `other` (every counter `>=`, at least one `>`).
    Newer,
    /// `other` strictly dominates `self` (every counter `<=`, at least one `<`).
    Older,
    /// Neither dominates the other (`self` is ahead on at least one key and `other` is ahead on
    /// at least one other key).
    Concurrent,
}

/// Per-`(member, incarnation)` monotonic counter map tracking the causal history of one path.
///
/// Missing entries are implicitly `0`, and [`VersionVector::set`] removes any entry set to `0` so
/// the representation is always canonical.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct VersionVector {
    counters: BTreeMap<VersionKey, u64>,
}

impl VersionVector {
    /// Creates an empty version vector (all counters implicitly `0`).
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a version vector with a single `(key, counter)` pair.
    pub fn singleton(key: VersionKey, counter: u64) -> Self {
        let mut vv = Self::new();
        vv.set(key, counter);
        vv
    }

    /// Constructs a canonical version vector from a raw map, dropping any zero entries.
    pub fn from_map(map: BTreeMap<VersionKey, u64>) -> Self {
        let counters = map.into_iter().filter(|(_, v)| *v > 0).collect();
        Self { counters }
    }

    /// Returns the counter for `key` (`0` if absent).
    pub fn get(&self, key: &VersionKey) -> u64 {
        self.counters.get(key).copied().unwrap_or(0)
    }

    /// Sets `key`'s counter to `value`, removing the entry if `value == 0`.
    pub fn set(&mut self, key: VersionKey, value: u64) {
        if value == 0 {
            self.counters.remove(&key);
        } else {
            self.counters.insert(key, value);
        }
    }

    /// Returns `true` if `self` and `other` share at least one `(member, incarnation)` key,
    /// meaning the two versioned histories descend from a common causal ancestor.
    pub fn shares_any_key(&self, other: &Self) -> bool {
        self.counters.keys().any(|k| other.counters.contains_key(k))
    }

    /// Removes `key` from the version vector, returning its previous counter if present.
    pub fn remove(&mut self, key: &VersionKey) -> Option<u64> {
        self.counters.remove(key)
    }

    /// Raises `key`'s counter to at least `value` without ever decreasing it.
    pub fn raise(&mut self, key: VersionKey, value: u64) {
        if value == 0 {
            return;
        }
        let entry = self.counters.entry(key).or_insert(0);
        if value > *entry {
            *entry = value;
        }
    }

    /// Advances `key`'s counter to strictly exceed both its current value and `floor`.
    ///
    /// Returns the new counter value.
    pub fn bump_above(&mut self, key: VersionKey, floor: u64) -> u64 {
        let current = self.counters.get(&key).copied().unwrap_or(0);
        let next = current.max(floor).saturating_add(1);
        self.counters.insert(key, next);
        next
    }

    /// Increments `key`'s counter by 1 and returns the new value.
    pub fn increment(&mut self, key: VersionKey) -> u64 {
        let next = self
            .counters
            .get(&key)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        self.counters.insert(key, next);
        next
    }

    /// Returns `true` if all counters are zero.
    pub fn is_empty(&self) -> bool {
        self.counters.is_empty()
    }

    /// Returns the number of keys with non-zero counters.
    pub fn len(&self) -> usize {
        self.counters.len()
    }

    /// Iterates over `(&VersionKey, counter)` pairs in deterministic order.
    pub fn iter(&self) -> impl Iterator<Item = (&VersionKey, u64)> {
        self.counters.iter().map(|(k, v)| (k, *v))
    }

    /// Borrows the underlying canonical counter map.
    pub fn as_map(&self) -> &BTreeMap<VersionKey, u64> {
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
                    self_greater = true;
                    it_a.next();
                }
                Ordering::Greater => {
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
        for (key, &val) in &other.counters {
            if val == 0 {
                continue;
            }
            let entry = self.counters.entry(key.clone()).or_insert(0);
            if val > *entry {
                *entry = val;
            }
        }
    }
}

impl fmt::Display for VersionVector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("{")?;
        for (i, (key, val)) in self.counters.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{key}:{val}")?;
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
        let raw = BTreeMap::<VersionKey, u64>::deserialize(deserializer)?;
        Ok(Self::from_map(raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_representation_drops_zeros() {
        let mut vv = VersionVector::new();
        let k_tower = VersionKey::new("tower", 1);
        let k_pi = VersionKey::new("pi", 2);
        vv.set(k_tower.clone(), 0);
        assert!(vv.is_empty());
        vv.set(k_tower.clone(), 3);
        assert_eq!(vv.get(&k_tower), 3);
        vv.set(k_tower.clone(), 0);
        assert!(vv.is_empty());
        assert_eq!(vv.get(&k_tower), 0);

        let from_json: VersionVector =
            serde_json::from_str(r#"{"tower#0000000000000001":0,"pi#0000000000000002":2}"#)
                .unwrap();
        assert_eq!(from_json, VersionVector::singleton(k_pi, 2));
    }

    #[test]
    fn compare_and_merge_basics() {
        let kt1 = VersionKey::new("tower", 1);
        let kt2 = VersionKey::new("tower", 2);
        let kp = VersionKey::new("pi", 1);
        let a = VersionVector::singleton(kt1.clone(), 2);
        let b = VersionVector::singleton(kt1.clone(), 1);
        assert_eq!(a.compare(&b), VersionOrder::Newer);
        assert_eq!(b.compare(&a), VersionOrder::Older);
        assert_eq!(a.compare(&a), VersionOrder::Equal);

        // Different incarnations of the same member are concurrent!
        let reincarnated = VersionVector::singleton(kt2.clone(), 1);
        assert_eq!(a.compare(&reincarnated), VersionOrder::Concurrent);

        let c = VersionVector::singleton(kp.clone(), 1);
        assert_eq!(a.compare(&c), VersionOrder::Concurrent);
        let merged = a.merge(&c);
        assert_eq!(merged.get(&kt1), 2);
        assert_eq!(merged.get(&kp), 1);
        assert_eq!(merged.compare(&a), VersionOrder::Newer);
        assert_eq!(merged.compare(&c), VersionOrder::Newer);
    }
}
