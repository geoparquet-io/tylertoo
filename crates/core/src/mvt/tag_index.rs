//! One first-encounter key/value index, shared by materialized and serialized layers.
use std::borrow::Cow;
use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};

use xxhash_rust::xxh3::Xxh3Builder;

use super::{PropertyValue, ScalarKey};

pub(super) struct TagIndex<'a> {
    keys: HashMap<Cow<'a, str>, u32, Xxh3Builder>,
    strings: HashMap<Cow<'a, str>, u32, Xxh3Builder>,
    scalars: HashMap<ScalarKey, u32, Xxh3Builder>,
    values: usize,
}

/// The tag maps' hasher: xxh3, which hashes long property strings several
/// times faster than std's `SipHash` (`SipHash` was 15.6% of a wide-property
/// export's cycles), keyed once per process with a secret drawn from
/// `RandomState`, so values crafted to collide still need the key.
#[expect(
    clippy::disallowed_types,
    reason = "#779: a process-wide hash key; no diagnostic globals"
)]
fn tag_hasher() -> Xxh3Builder {
    static KEYED: std::sync::OnceLock<Xxh3Builder> = std::sync::OnceLock::new();
    *KEYED.get_or_init(|| {
        let random = RandomState::new();
        let mut secret = [0u8; 192];
        for (i, word) in secret.as_chunks_mut::<8>().0.iter_mut().enumerate() {
            *word = random.hash_one(i).to_le_bytes();
        }
        Xxh3Builder::new().with_secret(secret)
    })
}

impl Default for TagIndex<'_> {
    fn default() -> Self {
        Self {
            keys: HashMap::with_hasher(tag_hasher()),
            strings: HashMap::with_hasher(tag_hasher()),
            scalars: HashMap::with_hasher(tag_hasher()),
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
}
