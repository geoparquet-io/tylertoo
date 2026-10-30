//! One first-encounter key/value index, shared by materialized and serialized layers.
use std::borrow::Cow;
use std::collections::HashMap;

use super::{PropertyValue, ScalarKey};

#[derive(Default)]
pub(super) struct TagIndex<'a> {
    keys: HashMap<Cow<'a, str>, u32>,
    strings: HashMap<Cow<'a, str>, u32>,
    scalars: HashMap<ScalarKey, u32>,
    values: usize,
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
