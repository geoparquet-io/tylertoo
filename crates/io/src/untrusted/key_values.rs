//! A Parquet footer's `key_value_metadata`, read and rewritten in place at
//! the thrift compact level.
//!
//! [`footer_key_values`] lists the entries of `FileMetaData` field 5 with
//! the byte span each one occupies. [`FooterKeyValues::rewrite`] rebuilds
//! that one field and copies every other byte of the footer verbatim, so a
//! rewrite that keeps every entry and appends none gives back the footer it
//! read. Re-encoding through parquet-rs instead would normalize the whole
//! footer (field order, statistics, unknown fields) and lose that property.

use std::ops::Range;

use super::{precheck_footer, read_as, Skim, SkimStop};
use crate::Error as InputError;

/// The thrift compact type of a struct, and of a binary.
const STRUCT: u8 = 12;
const BINARY: u8 = 8;
/// `FileMetaData.key_value_metadata`.
const KEY_VALUE_FIELD: i16 = 5;

/// One `KeyValue` entry of a footer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FooterKeyValue {
    /// The entry's key, UTF-8 decoded lossily.
    pub key: String,
    /// The entry's value bytes, `None` when the entry has no value.
    pub value: Option<Vec<u8>>,
    /// The entry's bytes in the footer, for a verbatim copy.
    span: Range<usize>,
}

/// The `key_value_metadata` field of a footer: its entries, and where the
/// field's list sits in the footer bytes.
#[derive(Debug, Clone)]
pub struct FooterKeyValues {
    /// The entries in footer order.
    pub entries: Vec<FooterKeyValue>,
    /// The list (header and elements) that is field 5's value.
    list: Range<usize>,
}

impl FooterKeyValues {
    /// The value of the last entry named `key`.
    pub fn get(&self, key: &str) -> Option<&[u8]> {
        self.entries
            .iter()
            .rev()
            .find(|e| e.key == key)
            .and_then(|e| e.value.as_deref())
    }

    /// The footer with field 5 rebuilt: the entries `keep` accepts, copied
    /// verbatim and in order, then `append` as new last entries. Every byte
    /// outside field 5's list is copied unchanged.
    pub fn rewrite(
        &self,
        thrift: &[u8],
        keep: impl Fn(&FooterKeyValue) -> bool,
        append: &[(&str, &[u8])],
    ) -> Vec<u8> {
        let kept: Vec<&FooterKeyValue> = self.entries.iter().filter(|e| keep(e)).collect();
        let mut out = Vec::with_capacity(thrift.len() + 64 * append.len());
        out.extend_from_slice(&thrift[..self.list.start]);
        list_header(kept.len() + append.len(), &mut out);
        for e in kept {
            out.extend_from_slice(&thrift[e.span.clone()]);
        }
        for (key, value) in append {
            // KeyValue { 1: key, 2: value }, both binary, then the stop byte.
            out.push(0x10 | BINARY);
            binary(key.as_bytes(), &mut out);
            out.push(0x10 | BINARY);
            binary(value, &mut out);
            out.push(0);
        }
        out.extend_from_slice(&thrift[self.list.end..]);
        out
    }
}

/// The `key_value_metadata` of the footer `thrift` (the `FileMetaData`
/// bytes between the data and the 8-byte trailer), or `None` when the
/// footer has no such field.
///
/// # Errors
///
/// Returns [`InputError::HostileFooter`] when the footer fails
/// [`precheck_footer`], or when its bytes cannot be walked to field 5's end.
pub fn footer_key_values(thrift: &[u8]) -> Result<Option<FooterKeyValues>, InputError> {
    precheck_footer(thrift)?;
    let total = thrift.len();
    let mut skim = Skim { wire: thrift };
    let mut found = None;
    let walked = skim.fields(0, |s, id, ty| {
        if id != KEY_VALUE_FIELD {
            return s.skip(ty, 1);
        }
        read_as(ty, 9)?;
        let start = total - s.wire.len();
        let (elem, size) = s.list_header()?;
        if elem != STRUCT && size > 0 {
            return Err(SkimStop::Hostile(
                "key_value_metadata is not a list of structs".into(),
            ));
        }
        let mut entries = Vec::with_capacity(size);
        for _ in 0..size {
            let at = total - s.wire.len();
            let (mut key, mut value) = (None, None);
            s.fields(2, |s, id, ty| match id {
                1 | 2 => {
                    read_as(ty, BINARY)?;
                    let len = s.varint()?;
                    let len = usize::try_from(len)
                        .ok()
                        .filter(|&n| n <= s.wire.len())
                        .ok_or(SkimStop::Short)?;
                    let bytes = s.wire[..len].to_vec();
                    s.advance(len as u64)?;
                    if id == 1 {
                        key = Some(bytes);
                    } else {
                        value = Some(bytes);
                    }
                    Ok(())
                }
                _ => s.skip(ty, 3),
            })?;
            entries.push(FooterKeyValue {
                key: String::from_utf8_lossy(&key.unwrap_or_default()).into_owned(),
                value,
                span: at..total - s.wire.len(),
            });
        }
        found = Some(FooterKeyValues {
            entries,
            list: start..total - s.wire.len(),
        });
        Ok(())
    });
    match walked {
        Ok(()) => Ok(found),
        Err(SkimStop::Hostile(reason)) => Err(InputError::HostileFooter(reason)),
        Err(SkimStop::Confused | SkimStop::Short) => Err(InputError::HostileFooter(
            "the footer's thrift cannot be walked to the end of key_value_metadata".into(),
        )),
    }
}

/// A compact-protocol list header for `n` structs.
fn list_header(n: usize, out: &mut Vec<u8>) {
    if n < 15 {
        out.push(((n as u8) << 4) | STRUCT);
    } else {
        out.push(0xf0 | STRUCT);
        varint(n as u64, out);
    }
}

fn binary(bytes: &[u8], out: &mut Vec<u8>) {
    varint(bytes.len() as u64, out);
    out.extend_from_slice(bytes);
}

fn varint(mut n: u64, out: &mut Vec<u8>) {
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use arrow_array::{Int32Array, RecordBatch};
    use arrow_schema::{DataType, Field, Schema};
    use parquet::arrow::ArrowWriter;
    use parquet::file::metadata::KeyValue;
    use parquet::file::properties::WriterProperties;

    /// A small Parquet file with `entries` as its key-value metadata.
    fn parquet_with(entries: &[(&str, &str)]) -> Vec<u8> {
        let schema = Arc::new(Schema::new(vec![Field::new("a", DataType::Int32, false)]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(Int32Array::from(vec![1, 2, 3]))],
        )
        .unwrap();
        let kv = entries
            .iter()
            .map(|(k, v)| KeyValue::new((*k).to_owned(), (*v).to_owned()))
            .collect();
        let props = WriterProperties::builder()
            .set_key_value_metadata(Some(kv))
            .build();
        let mut out = Vec::new();
        let mut w = ArrowWriter::try_new(&mut out, schema, Some(props)).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
        out
    }

    /// The footer thrift of a whole Parquet file.
    fn footer(file: &[u8]) -> &[u8] {
        let n = file.len();
        let len = u32::from_le_bytes(file[n - 8..n - 4].try_into().unwrap()) as usize;
        &file[n - 8 - len..n - 8]
    }

    fn keys(thrift: &[u8]) -> Vec<(String, Option<String>)> {
        crate::decode_footer(thrift)
            .unwrap()
            .file_metadata()
            .key_value_metadata()
            .unwrap()
            .iter()
            .map(|kv| (kv.key.clone(), kv.value.clone()))
            .collect()
    }

    #[test]
    fn lists_entries_in_footer_order() {
        let file = parquet_with(&[("geo", "{}"), ("other", "x")]);
        let kv = footer_key_values(footer(&file)).unwrap().unwrap();
        let got: Vec<_> = kv.entries.iter().map(|e| e.key.as_str()).collect();
        // ArrowWriter adds ARROW:schema after the user's entries.
        assert_eq!(&got[..2], ["geo", "other"]);
        assert_eq!(kv.get("other"), Some(&b"x"[..]));
        assert_eq!(kv.get("missing"), None);
    }

    #[test]
    fn keeping_everything_gives_back_the_same_footer() {
        let file = parquet_with(&[("geo", "{}")]);
        let thrift = footer(&file);
        let kv = footer_key_values(thrift).unwrap().unwrap();
        assert_eq!(kv.rewrite(thrift, |_| true, &[]), thrift);
    }

    #[test]
    fn an_appended_entry_comes_last_and_the_rest_decodes_unchanged() {
        let file = parquet_with(&[("geo", "{}")]);
        let thrift = footer(&file);
        let kv = footer_key_values(thrift).unwrap().unwrap();
        let out = kv.rewrite(thrift, |_| true, &[("geo:pmtiles", b"{\"a\":1}")]);
        let mut want = keys(thrift);
        want.push(("geo:pmtiles".into(), Some("{\"a\":1}".into())));
        assert_eq!(keys(&out), want);
        let before = crate::decode_footer(thrift).unwrap();
        let after = crate::decode_footer(&out).unwrap();
        assert_eq!(before.num_row_groups(), after.num_row_groups());
        assert_eq!(
            before.file_metadata().schema_descr().num_columns(),
            after.file_metadata().schema_descr().num_columns()
        );
        // Removing it again restores the original bytes.
        let back = footer_key_values(&out).unwrap().unwrap();
        assert_eq!(back.rewrite(&out, |e| e.key != "geo:pmtiles", &[]), thrift);
    }

    #[test]
    fn a_long_list_header_round_trips() {
        let many: Vec<(String, String)> = (0..20).map(|i| (format!("k{i}"), "v".into())).collect();
        let refs: Vec<(&str, &str)> = many.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let file = parquet_with(&refs);
        let thrift = footer(&file);
        let kv = footer_key_values(thrift).unwrap().unwrap();
        let out = kv.rewrite(thrift, |_| true, &[("x", b"y")]);
        assert_eq!(keys(&out).len(), keys(thrift).len() + 1);
        let back = footer_key_values(&out).unwrap().unwrap();
        assert_eq!(back.rewrite(&out, |e| e.key != "x", &[]), thrift);
    }

    #[test]
    fn a_truncated_footer_is_refused() {
        let file = parquet_with(&[("geo", "{}")]);
        let thrift = footer(&file);
        let kv = footer_key_values(thrift).unwrap().unwrap();
        // Cut inside the key-value list.
        let cut = &thrift[..kv.list.start + 3];
        assert!(matches!(
            footer_key_values(cut),
            Err(InputError::HostileFooter(_))
        ));
    }

    #[test]
    fn a_footer_without_key_values_has_none() {
        // FileMetaData with only version (field 1, i32 = 1) and a stop byte.
        assert!(footer_key_values(&[0x15, 0x02, 0x00]).unwrap().is_none());
    }
}
