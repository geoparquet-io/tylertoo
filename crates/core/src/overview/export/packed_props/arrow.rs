//! Arrow-to-packed writers. No per-row owned property values or column value `Vec`s.
use std::{cell::RefCell, fmt::Write as _, io::Write as _, sync::Arc};

use arrow_array::{cast::AsArray, types::*, Array, ArrayRef};
use arrow_cast::display::{ArrayFormatter, FormatOptions};
use arrow_schema::DataType;

use super::super::{
    formattable_temporal_type, is_json_encoded, is_string_type, is_temporal_scalar, write_json_str,
};
use super::{append_value, wire_len, PackedProps};
use crate::mvt::PropertyRef;
use crate::overview::export::ExportError;

/// Only this module can write its bytes. Every text writer below writes &str,
/// Arrow's UTF-8 Display, or `serde_json`'s UTF-8 output. This carries the proof
/// required by `PackedProps`' trusted traversal through finalization.
#[derive(Default)]
pub(in crate::overview::export) struct Row {
    bytes: Vec<u8>,
    /// Entries written: one per schema column.
    columns: usize,
}

impl Row {
    pub(in crate::overview::export) fn finish(self, names: Arc<[Arc<str>]>) -> PackedProps {
        // The trusted scan reads one tag per name; a table of another width
        // would misparse the bytes into an unvalidated UTF-8 view.
        assert_eq!(
            self.columns,
            names.len(),
            "row written for {} columns finished under a schema table of {}",
            self.columns,
            names.len()
        );
        PackedProps {
            names,
            data: self.bytes.into(),
        }
    }

    fn value(&mut self, value: Option<PropertyRef<'_>>) -> Result<(), ExportError> {
        self.columns += 1;
        append_value(&mut self.bytes, value)
    }

    /// Callers are only the typed JSON/Display writers in this module.
    fn text(&mut self, write: impl FnOnce(&mut Vec<u8>)) -> Result<(), ExportError> {
        self.columns += 1;
        self.bytes.push(1);
        let length = self.bytes.len();
        self.bytes.extend_from_slice(&[0; 4]);
        write(&mut self.bytes);
        let len = wire_len(self.bytes.len() - length - 4)?;
        self.bytes[length..length + 4].copy_from_slice(&len.to_le_bytes());
        Ok(())
    }
}

pub(in crate::overview::export) fn extract<'a>(
    columns: impl Iterator<Item = &'a dyn Array>,
    rows: usize,
) -> Result<Vec<Row>, ExportError> {
    let mut out: Vec<Row> = (0..rows).map(|_| Row::default()).collect();
    for col in columns {
        append_column(col, &mut out)?;
    }
    // The spill frames a row with a u32 length.
    for row in &out {
        wire_len(row.bytes.len())?;
    }
    Ok(out)
}

type ScalarReader = for<'a> fn(&'a dyn Array, usize) -> Option<PropertyRef<'a>>;

fn scalar_reader(dt: &DataType) -> Option<ScalarReader> {
    macro_rules! primitive {
        ($ty:ty, $variant:ident, $cast:ty) => {
            Some(|a, i| {
                (!a.is_null(i))
                    .then(|| PropertyRef::$variant(a.as_primitive::<$ty>().value(i) as $cast))
            })
        };
    }
    match dt {
        DataType::Utf8 => {
            Some(|a, i| (!a.is_null(i)).then(|| PropertyRef::String(a.as_string::<i32>().value(i))))
        }
        DataType::LargeUtf8 => {
            Some(|a, i| (!a.is_null(i)).then(|| PropertyRef::String(a.as_string::<i64>().value(i))))
        }
        DataType::Boolean => {
            Some(|a, i| (!a.is_null(i)).then(|| PropertyRef::Bool(a.as_boolean().value(i))))
        }
        DataType::Int8 => primitive!(Int8Type, Int, i64),
        DataType::Int16 => primitive!(Int16Type, Int, i64),
        DataType::Int32 => primitive!(Int32Type, Int, i64),
        DataType::Int64 => primitive!(Int64Type, Int, i64),
        DataType::UInt8 => primitive!(UInt8Type, UInt, u64),
        DataType::UInt16 => primitive!(UInt16Type, UInt, u64),
        DataType::UInt32 => primitive!(UInt32Type, UInt, u64),
        DataType::UInt64 => primitive!(UInt64Type, UInt, u64),
        DataType::Float32 => primitive!(Float32Type, Float, f32),
        DataType::Float64 => primitive!(Float64Type, Double, f64),
        DataType::Decimal128(_, _) => Some(decimal128),
        _ => None,
    }
}

fn decimal128(col: &dyn Array, i: usize) -> Option<PropertyRef<'_>> {
    if col.is_null(i) {
        return None;
    }
    let DataType::Decimal128(_, scale) = col.data_type() else {
        unreachable!()
    };
    let scale = i32::from(*scale);
    let raw = col.as_primitive::<Decimal128Type>().value(i);
    let exact = (scale <= 0)
        .then(|| {
            10i128
                .checked_pow((-scale) as u32)
                .and_then(|m| raw.checked_mul(m))
        })
        .flatten()
        .and_then(|v| i64::try_from(v).ok());
    Some(match exact {
        Some(v) => PropertyRef::Int(v),
        None => PropertyRef::Double(raw as f64 / 10f64.powi(scale)),
    })
}

fn normalized_temporal(col: &dyn Array) -> Option<ArrayRef> {
    let dt = formattable_temporal_type(col.data_type());
    (dt != *col.data_type())
        .then(|| {
            col.to_data()
                .into_builder()
                .data_type(dt)
                .build()
                .ok()
                .map(arrow_array::make_array)
        })
        .flatten()
}

fn append_column(col: &dyn Array, rows: &mut [Row]) -> Result<(), ExportError> {
    if let Some(read) = scalar_reader(col.data_type()) {
        for (i, row) in rows.iter_mut().enumerate() {
            row.value(read(col, i))?;
        }
        return Ok(());
    }
    let options = FormatOptions::default();
    if is_temporal_scalar(col.data_type()) || matches!(col.data_type(), DataType::Decimal256(_, _))
    {
        let normalized = normalized_temporal(col);
        let target = normalized.as_deref().unwrap_or(col);
        let fmt = match ArrayFormatter::try_new(target, &options) {
            Ok(fmt) => Some(fmt),
            Err(e) => {
                if is_temporal_scalar(col.data_type()) {
                    log::warn!(
                        "cannot format {:?} column as text, dropping it: {e}",
                        col.data_type()
                    );
                }
                None
            }
        };
        let number = matches!(col.data_type(), DataType::Decimal256(_, _));
        let mut scratch = String::new();
        for (i, row) in rows.iter_mut().enumerate() {
            let Some(fmt) = fmt.as_ref().filter(|_| !col.is_null(i)) else {
                row.value(None)?;
                continue;
            };
            if number {
                scratch.clear();
                write!(&mut scratch, "{}", fmt.value(i)).unwrap();
                row.value(scratch.parse().ok().map(PropertyRef::Double))?;
            } else {
                row.text(|out| write!(out, "{}", fmt.value(i)).unwrap())?;
            }
        }
        return Ok(());
    }
    if let DataType::Dictionary(_, value) = col.data_type() {
        match arrow_cast::cast(col, value) {
            Ok(flat) => {
                return append_column(flat.as_ref(), rows);
            }
            Err(e) => log::warn!(
                "cannot unwrap {:?} column, dropping it: {e}",
                col.data_type()
            ),
        }
    } else if is_json_encoded(col.data_type()) {
        let node = JsonNode::resolve(col, &options);
        for (i, row) in rows.iter_mut().enumerate() {
            if col.is_null(i) {
                row.value(None)?;
            } else {
                row.text(|out| node.write(out, i))?;
            }
        }
        return Ok(());
    }
    for row in rows {
        row.value(None)?;
    }
    Ok(())
}

/// Borrowed Arrow tree: it resolves structure once but never materializes leaf
/// values. Offsets address the original children, so sliced lists remain cheap.
enum JsonNode<'a> {
    Scalar {
        array: &'a dyn Array,
        read: ScalarReader,
    },
    Formatted {
        array: &'a dyn Array,
        fmt: ArrayFormatter<'a>,
        number: bool,
        scratch: RefCell<String>,
    },
    /// An array this node owns (a cast result), read like `Scalar`.
    Owned {
        array: ArrayRef,
        read: ScalarReader,
    },
    Struct {
        array: &'a arrow_array::StructArray,
        fields: Vec<(&'a str, JsonNode<'a>)>,
    },
    List {
        array: &'a dyn Array,
        offsets: Offsets<'a>,
        values: Box<JsonNode<'a>>,
    },
    Map {
        array: &'a arrow_array::MapArray,
        string_keys: bool,
        keys: Box<JsonNode<'a>>,
        values: Box<JsonNode<'a>>,
    },
    Dictionary(ArrayRef),
    Null,
}

enum Offsets<'a> {
    Small(&'a [i32]),
    Large(&'a [i64]),
    Fixed(usize),
}
impl Offsets<'_> {
    fn range(&self, i: usize) -> std::ops::Range<usize> {
        match self {
            Self::Small(o) => o[i] as usize..o[i + 1] as usize,
            Self::Large(o) => o[i] as usize..o[i + 1] as usize,
            Self::Fixed(w) => i * w..(i + 1) * w,
        }
    }
}

impl<'a> JsonNode<'a> {
    fn resolve(col: &'a dyn Array, options: &'a FormatOptions<'a>) -> Self {
        if let Some(read) = scalar_reader(col.data_type()) {
            return Self::Scalar { array: col, read };
        }
        match col.data_type() {
            DataType::Struct(_) => {
                let a = col.as_struct();
                Self::Struct {
                    array: a,
                    fields: a
                        .fields()
                        .iter()
                        .zip(a.columns())
                        .map(|(f, c)| (f.name().as_str(), Self::resolve(c.as_ref(), options)))
                        .collect(),
                }
            }
            DataType::List(_) => {
                let a = col.as_list::<i32>();
                Self::List {
                    array: col,
                    offsets: Offsets::Small(a.value_offsets()),
                    values: Box::new(Self::resolve(a.values().as_ref(), options)),
                }
            }
            DataType::LargeList(_) => {
                let a = col.as_list::<i64>();
                Self::List {
                    array: col,
                    offsets: Offsets::Large(a.value_offsets()),
                    values: Box::new(Self::resolve(a.values().as_ref(), options)),
                }
            }
            DataType::FixedSizeList(_, _) => {
                let a = col.as_fixed_size_list();
                Self::List {
                    array: col,
                    offsets: Offsets::Fixed(a.value_length() as usize),
                    values: Box::new(Self::resolve(a.values().as_ref(), options)),
                }
            }
            DataType::Map(_, _) => {
                let a = col.as_map();
                Self::Map {
                    array: a,
                    string_keys: is_string_type(a.keys().data_type()),
                    keys: Box::new(Self::resolve(a.keys().as_ref(), options)),
                    values: Box::new(Self::resolve(a.values().as_ref(), options)),
                }
            }
            DataType::Dictionary(_, value) => match arrow_cast::cast(col, value) {
                Ok(flat) => match scalar_reader(flat.data_type()) {
                    Some(read) => Self::Owned { array: flat, read },
                    // A struct, list, map or temporal under a dictionary is rare:
                    // it resolves per value.
                    None => Self::Dictionary(flat),
                },
                Err(_) => Self::Null,
            },
            dt if is_temporal_scalar(dt) || matches!(dt, DataType::Decimal256(_, _)) => {
                let number = matches!(dt, DataType::Decimal256(_, _));
                if let Some(array) = normalized_temporal(col) {
                    // Cast-to-text formats with the same default `FormatOptions`.
                    return arrow_cast::cast(&array, &DataType::Utf8)
                        .map(|array| Self::Owned {
                            array,
                            read: scalar_reader(&DataType::Utf8).unwrap(),
                        })
                        .unwrap_or(Self::Null);
                }
                match ArrayFormatter::try_new(col, options) {
                    Ok(fmt) => Self::Formatted {
                        array: col,
                        fmt,
                        number,
                        scratch: RefCell::new(String::new()),
                    },
                    Err(_) => Self::Null,
                }
            }
            _ => Self::Null,
        }
    }

    fn is_null(&self, i: usize) -> bool {
        match self {
            Self::Null => true,
            Self::Scalar { array, .. }
            | Self::Formatted { array, .. }
            | Self::List { array, .. } => array.is_null(i),
            Self::Owned { array, .. } | Self::Dictionary(array) => array.is_null(i),
            Self::Struct { array, .. } => array.is_null(i),
            Self::Map { array, .. } => array.is_null(i),
        }
    }

    fn write(&self, out: &mut Vec<u8>, i: usize) {
        if self.is_null(i) {
            out.extend_from_slice(b"null");
            return;
        }
        match self {
            Self::Null => unreachable!(),
            Self::Scalar { array, read } => write_scalar(out, read(*array, i)),
            Self::Formatted {
                fmt,
                number,
                scratch,
                ..
            } => formatted_json(out, fmt, i, *number, &mut scratch.borrow_mut()),
            Self::Owned { array, read } => write_scalar(out, read(array.as_ref(), i)),
            Self::Dictionary(array) => {
                let options = FormatOptions::default();
                // Resolve only this value: resolving the whole array per value is quadratic.
                JsonNode::resolve(array.slice(i, 1).as_ref(), &options).write(out, 0);
            }
            Self::Struct { fields, .. } => {
                out.push(b'{');
                for (k, (name, child)) in fields.iter().enumerate() {
                    if k > 0 {
                        out.push(b',');
                    }
                    write_json_str(out, name);
                    out.push(b':');
                    child.write(out, i);
                }
                out.push(b'}');
            }
            Self::List {
                offsets, values, ..
            } => {
                out.push(b'[');
                for (k, j) in offsets.range(i).enumerate() {
                    if k > 0 {
                        out.push(b',');
                    }
                    values.write(out, j);
                }
                out.push(b']');
            }
            Self::Map {
                array,
                string_keys,
                keys,
                values,
            } => {
                out.push(if *string_keys { b'{' } else { b'[' });
                let offsets = array.value_offsets();
                for (k, j) in (offsets[i] as usize..offsets[i + 1] as usize).enumerate() {
                    if k > 0 {
                        out.push(b',');
                    }
                    if *string_keys {
                        if keys.is_null(j) {
                            write_json_str(out, "null");
                        } else {
                            keys.write(out, j);
                        }
                        out.push(b':');
                        values.write(out, j);
                    } else {
                        out.push(b'[');
                        keys.write(out, j);
                        out.push(b',');
                        values.write(out, j);
                        out.push(b']');
                    }
                }
                out.push(if *string_keys { b'}' } else { b']' });
            }
        }
    }
}

fn formatted_json(
    out: &mut Vec<u8>,
    fmt: &ArrayFormatter<'_>,
    i: usize,
    number: bool,
    scratch: &mut String,
) {
    scratch.clear();
    write!(scratch, "{}", fmt.value(i)).unwrap();
    if number {
        write_scalar(out, scratch.parse().ok().map(PropertyRef::Double));
    } else {
        write_json_str(out, scratch);
    }
}

fn write_scalar(out: &mut Vec<u8>, value: Option<PropertyRef<'_>>) {
    let _ = match value {
        None => {
            out.extend_from_slice(b"null");
            return;
        }
        Some(PropertyRef::String(v)) => serde_json::to_writer(out, v),
        Some(PropertyRef::Float(v)) => serde_json::to_writer(out, &v),
        Some(PropertyRef::Double(v)) => serde_json::to_writer(out, &v),
        Some(PropertyRef::Int(v)) => serde_json::to_writer(out, &v),
        Some(PropertyRef::UInt(v)) => serde_json::to_writer(out, &v),
        Some(PropertyRef::Bool(v)) => serde_json::to_writer(out, &v),
    };
}

/// Differential check: the production writer against the owned oracle, on the
/// whole column and on a slice. Returns the oracle's values.
#[cfg(test)]
pub(in crate::overview::export) fn packed_column(
    col: &dyn Array,
) -> Vec<Option<crate::mvt::PropertyValue>> {
    use super::super::extract_property_column;
    let names: Arc<[Arc<str>]> = Arc::from(vec![Arc::from("property")]);
    let packed = extract(std::iter::once(col), col.len()).unwrap();
    let oracle = extract_property_column(col);
    for (i, (row, value)) in packed.into_iter().zip(oracle.clone()).enumerate() {
        let expected = PackedProps::pack(names.clone(), [value]);
        let actual = row.finish(names.clone());
        assert_eq!(
            actual.bytes(),
            expected.bytes(),
            "{:?}, row {i}",
            col.data_type()
        );
        actual.validate().unwrap();
        let reload = PackedProps::from_spill(names.clone(), Arc::from(actual.bytes())).unwrap();
        assert_eq!(reload.bytes(), expected.bytes());
    }
    if col.len() > 1 {
        let sliced = col.slice(1, col.len() - 1);
        let rows = extract(std::iter::once(sliced.as_ref()), sliced.len()).unwrap();
        for (row, value) in rows
            .into_iter()
            .zip(extract_property_column(sliced.as_ref()))
        {
            assert_eq!(
                row.finish(names.clone()).bytes(),
                PackedProps::pack(names.clone(), [value]).bytes(),
                "slice {:?}",
                col.data_type()
            );
        }
    }
    oracle
}

#[cfg(test)]
mod tests {
    use arrow_array::{builder::*, types::*, *};
    use arrow_buffer::i256;
    use arrow_schema::{DataType, Field};
    use std::sync::Arc;

    #[test]
    #[should_panic(expected = "schema table")]
    fn finishing_a_row_under_a_table_of_another_width_panics() {
        // The trusted scan walks one tag per table name; a table of another
        // width would misparse the bytes.
        let mut row = super::Row::default();
        row.value(Some(crate::mvt::PropertyRef::Int(1))).unwrap();
        let names: Arc<[Arc<str>]> = Arc::from(vec![Arc::from("a"), Arc::from("b")]);
        row.finish(names);
    }

    #[test]
    fn text_over_the_u32_wire_limit_is_an_error_not_a_panic() {
        assert_eq!(super::super::wire_len(5).unwrap(), 5);
        assert_eq!(super::super::wire_len(u32::MAX as usize).unwrap(), u32::MAX);
        assert!(super::super::wire_len(u32::MAX as usize + 1).is_err());
    }

    use super::packed_column;

    fn assert_column(col: &dyn Array) {
        packed_column(col);
    }

    #[test]
    fn direct_arrow_scalar_types_match_owned_oracle() {
        let cols: Vec<ArrayRef> = vec![
            Arc::new(StringArray::from(vec![Some("héllo\""), None, Some("")])),
            Arc::new(LargeStringArray::from(vec![Some("large"), None, Some("")])),
            Arc::new(BooleanArray::from(vec![Some(false), None, Some(true)])),
            Arc::new(Int8Array::from(vec![Some(i8::MIN), None, Some(i8::MAX)])),
            Arc::new(Int16Array::from(vec![Some(i16::MIN), None, Some(i16::MAX)])),
            Arc::new(Int32Array::from(vec![Some(i32::MIN), None, Some(i32::MAX)])),
            Arc::new(Int64Array::from(vec![Some(i64::MIN), None, Some(i64::MAX)])),
            Arc::new(UInt8Array::from(vec![Some(0), None, Some(u8::MAX)])),
            Arc::new(UInt16Array::from(vec![Some(0), None, Some(u16::MAX)])),
            Arc::new(UInt32Array::from(vec![Some(0), None, Some(u32::MAX)])),
            Arc::new(UInt64Array::from(vec![Some(0), None, Some(u64::MAX)])),
            Arc::new(Float32Array::from(vec![
                Some(-0.0),
                None,
                Some(f32::from_bits(0x7fc0_0042)),
            ])),
            Arc::new(Float64Array::from(vec![
                Some(f64::NEG_INFINITY),
                None,
                Some(f64::from_bits(0x7ff8_0000_0000_0042)),
            ])),
            Arc::new(
                Decimal128Array::from(vec![Some(9_007_199_254_740_993i128), None, Some(-99)])
                    .with_precision_and_scale(38, 0)
                    .unwrap(),
            ),
            Arc::new(
                Decimal128Array::from(vec![Some(150), None, Some(-99)])
                    .with_precision_and_scale(38, 2)
                    .unwrap(),
            ),
            Arc::new(
                Decimal128Array::from(vec![
                    Some(150),
                    None,
                    Some(99_999_999_999_999_999_999_999_999_999i128),
                ])
                .with_precision_and_scale(38, -3)
                .unwrap(),
            ),
            Arc::new(
                Decimal256Array::from(vec![
                    Some(i256::from_i128(150)),
                    None,
                    Some(i256::from_i128(-990)),
                ])
                .with_precision_and_scale(50, 2)
                .unwrap(),
            ),
            Arc::new(Date32Array::from(vec![Some(20710), None, Some(0)])),
            Arc::new(Date64Array::from(vec![
                Some(1_789_344_000_000),
                None,
                Some(0),
            ])),
            Arc::new(Time32SecondArray::from(vec![Some(45296), None, Some(0)])),
            Arc::new(Time32MillisecondArray::from(vec![
                Some(45296000),
                None,
                Some(0),
            ])),
            Arc::new(Time64MicrosecondArray::from(vec![
                Some(45296000000),
                None,
                Some(0),
            ])),
            Arc::new(Time64NanosecondArray::from(vec![
                Some(45296000000000),
                None,
                Some(0),
            ])),
            Arc::new(TimestampSecondArray::from(vec![
                Some(1_789_389_296),
                None,
                Some(0),
            ])),
            Arc::new(TimestampMillisecondArray::from(vec![
                Some(1_789_389_296_000),
                None,
                Some(0),
            ])),
            Arc::new(TimestampMicrosecondArray::from(vec![
                Some(1_789_389_296_000_000),
                None,
                Some(0),
            ])),
            Arc::new(TimestampNanosecondArray::from(vec![
                Some(1_789_389_296_000_000_000),
                None,
                Some(0),
            ])),
            Arc::new(NullArray::new(3)),
            Arc::new(BinaryArray::from(vec![
                Some(b"x".as_slice()),
                None,
                Some(b"".as_slice()),
            ])),
        ];
        for col in cols {
            assert_column(col.as_ref());
        }
        for tz in [
            "UTC",
            "utc",
            "Utc",
            "Z",
            "z",
            "+00:00",
            "-00:00",
            "00:00",
            "Europe/Paris",
        ] {
            let col = TimestampMicrosecondArray::from(vec![Some(1_789_389_296_000_000), None])
                .with_timezone(tz);
            assert_column(&col);
        }
        let dict: DictionaryArray<Int8Type> = [Some("a"), None, Some("b"), Some("a")]
            .into_iter()
            .collect();
        assert_column(&dict);
    }

    #[test]
    fn direct_arrow_nested_types_and_slices_match_owned_oracle() {
        let strings: ArrayRef = Arc::new(StringArray::from(vec![Some("héllo\""), None, Some("")]));
        let numbers: ArrayRef =
            Arc::new(Float64Array::from(vec![Some(-0.0), None, Some(f64::NAN)]));
        let temporal: ArrayRef = Arc::new(
            TimestampMicrosecondArray::from(vec![Some(1_789_389_296_000_000), None, Some(0)])
                .with_timezone("UTC"),
        );
        let fields = vec![
            Arc::new(Field::new("text", DataType::Utf8, true)),
            Arc::new(Field::new("number", DataType::Float64, true)),
            Arc::new(Field::new("time", temporal.data_type().clone(), true)),
        ];
        let structure = StructArray::new(fields.into(), vec![strings, numbers, temporal], None);
        assert_column(&structure);
        let dictionary = DictionaryArray::<Int8Type>::try_new(
            Int8Array::from(vec![Some(0), None, Some(2), Some(0)]),
            Arc::new(structure),
        )
        .unwrap();
        assert_column(&dictionary);
        let mut list = ListBuilder::new(Int32Builder::new());
        list.values().append_value(1);
        list.values().append_null();
        list.append(true);
        list.append(false);
        list.append(true);
        assert_column(&list.finish());
        let mut large = LargeListBuilder::new(StringBuilder::new());
        large.values().append_value("one");
        large.values().append_null();
        large.append(true);
        large.append(false);
        large.append(true);
        assert_column(&large.finish());
        let mut fixed = FixedSizeListBuilder::new(Float64Builder::new(), 2);
        fixed.values().append_value(f64::INFINITY);
        fixed.values().append_value(-0.0);
        fixed.append(true);
        fixed.values().append_null();
        fixed.values().append_null();
        fixed.append(false);
        fixed.values().append_value(1.5);
        fixed.values().append_value(f64::NAN);
        fixed.append(true);
        assert_column(&fixed.finish());
        let mut map = MapBuilder::new(None, StringBuilder::new(), Int32Builder::new());
        map.keys().append_value("say \"hi\"");
        map.values().append_value(1);
        map.append(true).unwrap();
        map.append(false).unwrap();
        map.append(true).unwrap();
        assert_column(&map.finish());
        let mut numeric_map = MapBuilder::new(None, Int32Builder::new(), StringBuilder::new());
        numeric_map.keys().append_value(1);
        numeric_map.values().append_value("one");
        numeric_map.keys().append_value(2);
        numeric_map.values().append_null();
        numeric_map.append(true).unwrap();
        numeric_map.append(false).unwrap();
        numeric_map.append(true).unwrap();
        assert_column(&numeric_map.finish());
    }

    #[test]
    fn nested_dictionary_temporal_and_decimal_leaves_match_owned_oracle() {
        // list<dictionary<utf8>>: the dictionary sits inside a list.
        let mut dict_list = ListBuilder::new(StringDictionaryBuilder::<Int8Type>::new());
        dict_list.values().append_value("a");
        dict_list.values().append_null();
        dict_list.values().append_value("a");
        dict_list.append(true);
        dict_list.append(false);
        dict_list.values().append_value("b");
        dict_list.append(true);
        assert_column(&dict_list.finish());

        // struct{d: date32, n: naive timestamp, x: decimal256, u: UTC timestamps}
        let mut fields = vec![
            Arc::new(Field::new("d", DataType::Date32, true)),
            Arc::new(Field::new(
                "n",
                DataType::Timestamp(arrow_schema::TimeUnit::Millisecond, None),
                true,
            )),
            Arc::new(Field::new("x", DataType::Decimal256(50, 2), true)),
        ];
        let mut cols: Vec<ArrayRef> = vec![
            Arc::new(Date32Array::from(vec![Some(20710), None, Some(0)])),
            Arc::new(TimestampMillisecondArray::from(vec![
                Some(1_789_389_296_123),
                None,
                Some(0),
            ])),
            Arc::new(
                Decimal256Array::from(vec![
                    Some(i256::from_i128(150)),
                    None,
                    Some(i256::from_i128(-990)),
                ])
                .with_precision_and_scale(50, 2)
                .unwrap(),
            ),
        ];
        // UTC timestamps in every unit and several UTC spellings, with nulls.
        let values = [Some(1_789_389_296i64), None, Some(0)];
        let times: Vec<(&str, ArrayRef)> = vec![
            (
                "s",
                Arc::new(TimestampSecondArray::from(values.to_vec()).with_timezone("UTC")),
            ),
            (
                "ms",
                Arc::new(
                    TimestampMillisecondArray::from(
                        values.map(|v| v.map(|v| v * 1000 + 7)).to_vec(),
                    )
                    .with_timezone("+00:00"),
                ),
            ),
            (
                "us",
                Arc::new(
                    TimestampMicrosecondArray::from(
                        values.map(|v| v.map(|v| v * 1_000_000 + 7)).to_vec(),
                    )
                    .with_timezone("Z"),
                ),
            ),
            (
                "ns",
                Arc::new(
                    TimestampNanosecondArray::from(
                        values.map(|v| v.map(|v| v * 1_000_000_000 + 7)).to_vec(),
                    )
                    .with_timezone("utc"),
                ),
            ),
        ];
        for (name, col) in times {
            fields.push(Arc::new(Field::new(name, col.data_type().clone(), true)));
            cols.push(col);
        }
        let structure = StructArray::new(fields.into(), cols, None);
        assert_column(&structure);
        // The same struct inside a list, and under a dictionary.
        let offsets = arrow_buffer::OffsetBuffer::new(vec![0, 2, 2, 3].into());
        let item = Arc::new(Field::new("item", structure.data_type().clone(), true));
        let list = ListArray::new(item, offsets, Arc::new(structure.clone()), None);
        assert_column(&list);
        let dictionary = DictionaryArray::<Int8Type>::try_new(
            Int8Array::from(vec![Some(0), None, Some(2), Some(0)]),
            Arc::new(structure),
        )
        .unwrap();
        assert_column(&dictionary);
    }

    #[test]
    fn large_dictionary_over_struct_with_utc_leaf_is_not_quadratic() {
        let n = 10_000_i64;
        let times = TimestampSecondArray::from((0..n).collect::<Vec<_>>()).with_timezone("utc");
        let field = Arc::new(Field::new("t", times.data_type().clone(), true));
        let structure = StructArray::new(vec![field].into(), vec![Arc::new(times)], None);
        let keys = Int32Array::from((0..n as i32).collect::<Vec<_>>());
        let dictionary = DictionaryArray::<Int32Type>::try_new(keys, Arc::new(structure)).unwrap();
        let start = std::time::Instant::now();
        assert_column(&dictionary);
        // Per-value resolves of the whole array took minutes here; this bound is generous.
        assert!(start.elapsed().as_secs() < 30, "{:?}", start.elapsed());
    }
}
