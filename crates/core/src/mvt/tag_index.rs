//! One first-encounter key/value index, shared by materialized and serialized layers.
use std::borrow::Cow;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher, RandomState};

use super::{PropertyValue, ScalarKey};

pub(super) struct TagIndex<'a> {
    keys: HashMap<Cow<'a, str>, u32, TagHash>,
    strings: HashMap<Cow<'a, str>, u32, TagHash>,
    scalars: HashMap<ScalarKey, u32, TagHash>,
    values: usize,
}

/// The tag maps' hasher: xxh3, which hashes long property strings several
/// times faster than std's `SipHash` (`SipHash` was 15.6% of a wide-property
/// export's cycles), seeded once per process from `RandomState`, so values
/// crafted to collide still need the seed. Each write is one seeded one-shot
/// xxh3 chained on the hash so far: a lookup builds an 8-byte hasher, not
/// xxhash-rust's streaming state with its secret and buffer.
#[derive(Clone, Copy)]
struct TagHash(u64);

impl TagHash {
    #[expect(
        clippy::disallowed_types,
        reason = "#779: a process-wide hash seed; no diagnostic globals"
    )]
    fn seeded() -> Self {
        static SEED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        Self(*SEED.get_or_init(|| RandomState::new().hash_one(0u8)))
    }
}

impl BuildHasher for TagHash {
    type Hasher = TagHash;
    fn build_hasher(&self) -> TagHash {
        *self
    }
}

impl Hasher for TagHash {
    fn write(&mut self, bytes: &[u8]) {
        self.0 = xxhash_rust::xxh3::xxh3_64_with_seed(bytes, self.0);
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

impl Default for TagIndex<'_> {
    fn default() -> Self {
        Self {
            keys: HashMap::with_hasher(TagHash::seeded()),
            strings: HashMap::with_hasher(TagHash::seeded()),
            scalars: HashMap::with_hasher(TagHash::seeded()),
            values: 0,
        }
    }
}

impl<'a> TagIndex<'a> {
    pub(super) fn value<'p>(
        &mut self,
        value: &'p PropertyValue,
        store: &impl Fn(&'p str) -> Cow<'a, str>,
        inserted: impl FnOnce(&PropertyValue),
    ) -> u32 {
        let existing = match value {
            PropertyValue::String(s) => self.strings.get(s.as_str()),
            _ => self.scalars.get(&ScalarKey::of(value)),
        };
        if let Some(&index) = existing {
            return index;
        }
        let index = self.values as u32;
        self.values += 1;
        inserted(value);
        match value {
            PropertyValue::String(s) => {
                self.strings.insert(store(s), index);
            }
            _ => {
                self.scalars.insert(ScalarKey::of(value), index);
            }
        }
        index
    }

    pub(super) fn tags<'p, K: AsRef<str>>(
        &mut self,
        properties: &'p [(K, PropertyValue)],
        store: impl Fn(&'p str) -> Cow<'a, str>,
        mut new_key: impl FnMut(&str),
        mut new_value: impl FnMut(&PropertyValue),
    ) -> Vec<u32> {
        let mut tags = Vec::with_capacity(properties.len() * 2);
        for (key, value) in properties {
            let key = key.as_ref();
            let key_index = if let Some(&i) = self.keys.get(key) {
                i
            } else {
                let i = self.keys.len() as u32;
                self.keys.insert(store(key), i);
                new_key(key);
                i
            };
            tags.push(key_index);
            tags.push(self.value(value, &store, &mut new_value));
        }
        tags
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tag interning hashes every key and string value of every feature of
    /// every tile, so the maps share one hasher seeded once per process
    /// (keyed, as `RandomState` is, so crafted values cannot collide on
    /// purpose), not a fresh `SipHash` state per map.
    #[test]
    fn tag_maps_share_one_process_seeded_hasher() {
        let (a, b) = (TagIndex::default(), TagIndex::default());
        let v = "a property value";
        assert_eq!(
            a.strings.hasher().hash_one(v),
            b.strings.hasher().hash_one(v)
        );
        assert_eq!(a.keys.hasher().hash_one(v), b.strings.hasher().hash_one(v));
    }

    /// One lookup builds one hasher: a keyed xxh3 streaming state carries its
    /// 192-byte secret and 256-byte buffer, copied per lookup, which cost
    /// more than `SipHash` on short values (moldova task-clock +1.5%). The
    /// hasher is a seed, nothing more.
    #[test]
    fn a_tag_lookup_builds_a_seed_sized_hasher() {
        let index = TagIndex::default();
        assert!(std::mem::size_of_val(&index.strings.hasher().build_hasher()) <= 8);
    }
}
