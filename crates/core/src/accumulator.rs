//! Accumulator system for attribute aggregation during feature merging.
//!
//! This module implements tippecanoe-compatible attribute accumulation operations
//! for combining properties when features are merged during tile generation.
//!
//! # Tippecanoe Compatibility
//!
//! This is a 1:1 port of tippecanoe's accumulator system. Key behaviors:
//! - Unspecified attributes are DROPPED (not preserved) - this matches tippecanoe
//! - Mean accumulator requires separate count tracking for running average
//! - Type mismatches: numeric ops on strings use 0.0, string ops on numbers convert
//!
//! # Supported Operations
//!
//! | Operation | Behavior |
//! |-----------|----------|
//! | `Sum`     | Add numeric values |
//! | `Product` | Multiply numeric values |
//! | `Mean`    | Running average with count tracking |
//! | `Max`     | Keep maximum numeric value |
//! | `Min`     | Keep minimum numeric value |
//! | `Concat`  | Concatenate strings directly |
//! | `Comma`   | Concatenate with comma separator |
//! | `Count`   | Count merged features |
//!
//! # Examples
//!
//! ```
//! use gpq_tiles_core::accumulator::{AccumulatorConfig, AccumulatorOp};
//! use gpq_tiles_core::wkb::PropertyValue;
//! use std::collections::HashMap;
//!
//! // Configure accumulators for population sum and name concatenation
//! let mut config = AccumulatorConfig::new();
//! config.set_operation("population", AccumulatorOp::Sum);
//! config.set_operation("names", AccumulatorOp::Comma);
//!
//! let mut target = HashMap::new();
//! target.insert("population".to_string(), PropertyValue::Int(100));
//! target.insert("names".to_string(), PropertyValue::String("Alice".to_string()));
//!
//! let source = HashMap::from([
//!     ("population".to_string(), PropertyValue::Int(50)),
//!     ("names".to_string(), PropertyValue::String("Bob".to_string())),
//! ]);
//!
//! config.accumulate(&mut target, &source);
//!
//! assert_eq!(target.get("population"), Some(&PropertyValue::Int(150)));
//! assert_eq!(target.get("names"), Some(&PropertyValue::String("Alice,Bob".to_string())));
//! ```

use crate::wkb::PropertyValue;
use std::collections::HashMap;

/// Accumulator operation types matching tippecanoe's behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AccumulatorOp {
    /// Add numeric values (strings treated as 0.0)
    Sum,
    /// Multiply numeric values (strings treated as 1.0)
    Product,
    /// Running average with count tracking
    Mean,
    /// Keep maximum numeric value
    Max,
    /// Keep minimum numeric value
    Min,
    /// Concatenate strings directly (no separator)
    Concat,
    /// Concatenate strings with comma separator
    Comma,
    /// Count merged features (increments for each accumulation)
    Count,
}

impl AccumulatorOp {
    /// Parse operation from string (for CLI).
    ///
    /// Returns None for unrecognized operation names.
    /// Accepts aliases like "avg" for "mean", "maximum" for "max", etc.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "sum" => Some(AccumulatorOp::Sum),
            "product" => Some(AccumulatorOp::Product),
            "mean" | "avg" | "average" => Some(AccumulatorOp::Mean),
            "max" | "maximum" => Some(AccumulatorOp::Max),
            "min" | "minimum" => Some(AccumulatorOp::Min),
            "concat" | "concatenate" => Some(AccumulatorOp::Concat),
            "comma" => Some(AccumulatorOp::Comma),
            "count" => Some(AccumulatorOp::Count),
            _ => None,
        }
    }

    /// Get the string representation of this operation.
    pub fn as_str(&self) -> &'static str {
        match self {
            AccumulatorOp::Sum => "sum",
            AccumulatorOp::Product => "product",
            AccumulatorOp::Mean => "mean",
            AccumulatorOp::Max => "max",
            AccumulatorOp::Min => "min",
            AccumulatorOp::Concat => "concat",
            AccumulatorOp::Comma => "comma",
            AccumulatorOp::Count => "count",
        }
    }
}

impl std::str::FromStr for AccumulatorOp {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| {
            format!(
                "Invalid accumulator operation: '{}'. Valid operations: sum, product, mean, max, min, concat, comma, count",
                s
            )
        })
    }
}

impl std::fmt::Display for AccumulatorOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Configuration for attribute accumulation during feature merging.
///
/// Stores per-attribute accumulator operations and tracks counts for mean calculations.
///
/// # Tippecanoe Behavior
///
/// - Only attributes with configured operations are kept in the result
/// - Attributes without operations are DROPPED (not preserved)
/// - This matches tippecanoe's `-ac` flag behavior
#[derive(Clone, Debug, Default)]
pub struct AccumulatorConfig {
    /// Per-attribute accumulator operations
    operations: HashMap<String, AccumulatorOp>,
    /// Track counts for mean calculation (attribute_name -> count)
    /// This is essential for correct running average calculation
    mean_counts: HashMap<String, u64>,
}

impl AccumulatorConfig {
    /// Create a new empty accumulator configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the accumulator operation for an attribute.
    pub fn set_operation(&mut self, attribute: &str, op: AccumulatorOp) {
        self.operations.insert(attribute.to_string(), op);
        // Initialize count for mean accumulators
        if op == AccumulatorOp::Mean {
            self.mean_counts.insert(attribute.to_string(), 1);
        }
    }

    /// Get the operation configured for an attribute, if any.
    pub fn get_operation(&self, attribute: &str) -> Option<AccumulatorOp> {
        self.operations.get(attribute).copied()
    }

    /// Check if any accumulators are configured.
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    /// Get the number of configured operations.
    pub fn len(&self) -> usize {
        self.operations.len()
    }

    /// Get configured attributes and their operations.
    pub fn operations(&self) -> impl Iterator<Item = (&str, AccumulatorOp)> {
        self.operations.iter().map(|(k, v)| (k.as_str(), *v))
    }

    /// Reset mean counts (call at the start of a new accumulation sequence).
    ///
    /// This is important when starting to accumulate a new group of features.
    pub fn reset_counts(&mut self) {
        for (attr, op) in &self.operations {
            if *op == AccumulatorOp::Mean {
                self.mean_counts.insert(attr.clone(), 1);
            }
        }
    }

    /// Accumulate source properties into target properties.
    ///
    /// # Tippecanoe Compatibility
    ///
    /// - Only attributes with configured operations are modified/kept
    /// - Unspecified attributes in target are NOT preserved (dropped)
    /// - Type coercion follows tippecanoe rules:
    ///   - Numeric ops on strings: use 0.0 (sum, product, mean) or skip (min, max)
    ///   - String ops on numbers: convert to string representation
    ///
    /// # Arguments
    ///
    /// * `target` - The target property map to accumulate into (modified in place)
    /// * `source` - The source property map to accumulate from
    pub fn accumulate(
        &mut self,
        target: &mut HashMap<String, PropertyValue>,
        source: &HashMap<String, PropertyValue>,
    ) {
        for (attr, op) in &self.operations.clone() {
            let target_val = target.get(attr);
            let source_val = source.get(attr);

            let result = match op {
                AccumulatorOp::Sum => self.accumulate_sum(target_val, source_val),
                AccumulatorOp::Product => self.accumulate_product(target_val, source_val),
                AccumulatorOp::Mean => self.accumulate_mean(attr, target_val, source_val),
                AccumulatorOp::Max => self.accumulate_max(target_val, source_val),
                AccumulatorOp::Min => self.accumulate_min(target_val, source_val),
                AccumulatorOp::Concat => self.accumulate_concat(target_val, source_val),
                AccumulatorOp::Comma => self.accumulate_comma(target_val, source_val),
                AccumulatorOp::Count => self.accumulate_count(target_val),
            };

            if let Some(value) = result {
                target.insert(attr.clone(), value);
            }
        }

        // Remove attributes without configured operations (tippecanoe behavior)
        target.retain(|k, _| self.operations.contains_key(k));
    }

    // ========================================================================
    // Private accumulator implementations
    // ========================================================================

    /// Extract numeric value from PropertyValue.
    /// Returns 0.0 for non-numeric types (tippecanoe behavior).
    fn to_numeric(value: Option<&PropertyValue>) -> f64 {
        match value {
            Some(PropertyValue::Float(f)) => *f,
            Some(PropertyValue::Int(i)) => *i as f64,
            Some(PropertyValue::UInt(u)) => *u as f64,
            Some(PropertyValue::Bool(b)) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            Some(PropertyValue::String(_)) | None => 0.0,
        }
    }

    /// Extract numeric value, returning None for strings.
    /// Used for min/max where we skip non-numeric values.
    fn to_numeric_strict(value: Option<&PropertyValue>) -> Option<f64> {
        match value {
            Some(PropertyValue::Float(f)) => Some(*f),
            Some(PropertyValue::Int(i)) => Some(*i as f64),
            Some(PropertyValue::UInt(u)) => Some(*u as f64),
            Some(PropertyValue::Bool(b)) => Some(if *b { 1.0 } else { 0.0 }),
            Some(PropertyValue::String(_)) | None => None,
        }
    }

    /// Extract string value from PropertyValue.
    /// Converts numbers to string representation.
    fn to_string(value: Option<&PropertyValue>) -> String {
        match value {
            Some(PropertyValue::String(s)) => s.clone(),
            Some(PropertyValue::Int(i)) => i.to_string(),
            Some(PropertyValue::UInt(u)) => u.to_string(),
            Some(PropertyValue::Float(f)) => f.to_string(),
            Some(PropertyValue::Bool(b)) => b.to_string(),
            None => String::new(),
        }
    }

    /// Create PropertyValue from f64, preserving integer type if possible.
    fn from_numeric(value: f64) -> PropertyValue {
        // If the value is a whole number that fits in i64, use Int
        if value.fract() == 0.0 && value >= i64::MIN as f64 && value <= i64::MAX as f64 {
            PropertyValue::Int(value as i64)
        } else {
            PropertyValue::Float(value)
        }
    }

    fn accumulate_sum(
        &self,
        target: Option<&PropertyValue>,
        source: Option<&PropertyValue>,
    ) -> Option<PropertyValue> {
        let t = Self::to_numeric(target);
        let s = Self::to_numeric(source);
        Some(Self::from_numeric(t + s))
    }

    fn accumulate_product(
        &self,
        target: Option<&PropertyValue>,
        source: Option<&PropertyValue>,
    ) -> Option<PropertyValue> {
        // For product, default to 1.0 if missing (identity for multiplication)
        let t = match target {
            Some(v) => Self::to_numeric(Some(v)),
            None => 1.0,
        };
        let s = match source {
            Some(v) => Self::to_numeric(Some(v)),
            None => 1.0,
        };
        Some(Self::from_numeric(t * s))
    }

    fn accumulate_mean(
        &mut self,
        attr: &str,
        target: Option<&PropertyValue>,
        source: Option<&PropertyValue>,
    ) -> Option<PropertyValue> {
        let t = Self::to_numeric(target);
        let s = Self::to_numeric(source);

        // Get current count and increment
        let count = self.mean_counts.entry(attr.to_string()).or_insert(1);
        let old_count = *count;
        *count += 1;

        // Running average formula: new_mean = old_mean + (new_value - old_mean) / new_count
        // This is equivalent to: new_mean = (old_mean * old_count + new_value) / new_count
        let new_mean = (t * old_count as f64 + s) / *count as f64;
        Some(PropertyValue::Float(new_mean))
    }

    fn accumulate_max(
        &self,
        target: Option<&PropertyValue>,
        source: Option<&PropertyValue>,
    ) -> Option<PropertyValue> {
        let t = Self::to_numeric_strict(target);
        let s = Self::to_numeric_strict(source);

        match (t, s) {
            (Some(tv), Some(sv)) => Some(Self::from_numeric(tv.max(sv))),
            (Some(tv), None) => Some(Self::from_numeric(tv)),
            (None, Some(sv)) => Some(Self::from_numeric(sv)),
            (None, None) => None,
        }
    }

    fn accumulate_min(
        &self,
        target: Option<&PropertyValue>,
        source: Option<&PropertyValue>,
    ) -> Option<PropertyValue> {
        let t = Self::to_numeric_strict(target);
        let s = Self::to_numeric_strict(source);

        match (t, s) {
            (Some(tv), Some(sv)) => Some(Self::from_numeric(tv.min(sv))),
            (Some(tv), None) => Some(Self::from_numeric(tv)),
            (None, Some(sv)) => Some(Self::from_numeric(sv)),
            (None, None) => None,
        }
    }

    fn accumulate_concat(
        &self,
        target: Option<&PropertyValue>,
        source: Option<&PropertyValue>,
    ) -> Option<PropertyValue> {
        let t = Self::to_string(target);
        let s = Self::to_string(source);
        Some(PropertyValue::String(format!("{}{}", t, s)))
    }

    fn accumulate_comma(
        &self,
        target: Option<&PropertyValue>,
        source: Option<&PropertyValue>,
    ) -> Option<PropertyValue> {
        let t = Self::to_string(target);
        let s = Self::to_string(source);

        if t.is_empty() {
            Some(PropertyValue::String(s))
        } else if s.is_empty() {
            Some(PropertyValue::String(t))
        } else {
            Some(PropertyValue::String(format!("{},{}", t, s)))
        }
    }

    fn accumulate_count(&self, target: Option<&PropertyValue>) -> Option<PropertyValue> {
        let current = match target {
            Some(PropertyValue::Int(i)) => *i,
            Some(PropertyValue::UInt(u)) => *u as i64,
            _ => 0,
        };
        Some(PropertyValue::Int(current + 1))
    }
}

// ============================================================================
// Tests (TDD - these define the expected behavior)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // AccumulatorOp Parsing Tests
    // ========================================================================

    #[test]
    fn test_op_from_str_sum() {
        assert_eq!(AccumulatorOp::parse("sum"), Some(AccumulatorOp::Sum));
        assert_eq!(AccumulatorOp::parse("SUM"), Some(AccumulatorOp::Sum));
        assert_eq!(AccumulatorOp::parse("Sum"), Some(AccumulatorOp::Sum));
    }

    #[test]
    fn test_op_from_str_product() {
        assert_eq!(
            AccumulatorOp::parse("product"),
            Some(AccumulatorOp::Product)
        );
        assert_eq!(
            AccumulatorOp::parse("PRODUCT"),
            Some(AccumulatorOp::Product)
        );
    }

    #[test]
    fn test_op_from_str_mean_variants() {
        assert_eq!(AccumulatorOp::parse("mean"), Some(AccumulatorOp::Mean));
        assert_eq!(AccumulatorOp::parse("avg"), Some(AccumulatorOp::Mean));
        assert_eq!(AccumulatorOp::parse("average"), Some(AccumulatorOp::Mean));
    }

    #[test]
    fn test_op_from_str_max_variants() {
        assert_eq!(AccumulatorOp::parse("max"), Some(AccumulatorOp::Max));
        assert_eq!(AccumulatorOp::parse("maximum"), Some(AccumulatorOp::Max));
    }

    #[test]
    fn test_op_from_str_min_variants() {
        assert_eq!(AccumulatorOp::parse("min"), Some(AccumulatorOp::Min));
        assert_eq!(AccumulatorOp::parse("minimum"), Some(AccumulatorOp::Min));
    }

    #[test]
    fn test_op_from_str_concat_variants() {
        assert_eq!(AccumulatorOp::parse("concat"), Some(AccumulatorOp::Concat));
        assert_eq!(
            AccumulatorOp::parse("concatenate"),
            Some(AccumulatorOp::Concat)
        );
    }

    #[test]
    fn test_op_from_str_comma() {
        assert_eq!(AccumulatorOp::parse("comma"), Some(AccumulatorOp::Comma));
    }

    #[test]
    fn test_op_from_str_count() {
        assert_eq!(AccumulatorOp::parse("count"), Some(AccumulatorOp::Count));
    }

    #[test]
    fn test_op_from_str_invalid() {
        assert_eq!(AccumulatorOp::parse("invalid"), None);
        assert_eq!(AccumulatorOp::parse("first"), None);
        assert_eq!(AccumulatorOp::parse("last"), None);
    }

    #[test]
    fn test_op_as_str_roundtrip() {
        let ops = [
            AccumulatorOp::Sum,
            AccumulatorOp::Product,
            AccumulatorOp::Mean,
            AccumulatorOp::Max,
            AccumulatorOp::Min,
            AccumulatorOp::Concat,
            AccumulatorOp::Comma,
            AccumulatorOp::Count,
        ];

        for op in ops {
            let s = op.as_str();
            let parsed = AccumulatorOp::parse(s);
            assert_eq!(parsed, Some(op), "Round-trip failed for {:?}", op);
        }
    }

    // ========================================================================
    // AccumulatorConfig Basic Tests
    // ========================================================================

    #[test]
    fn test_config_new_is_empty() {
        let config = AccumulatorConfig::new();
        assert!(config.is_empty());
        assert_eq!(config.len(), 0);
    }

    #[test]
    fn test_config_set_operation() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("population", AccumulatorOp::Sum);

        assert!(!config.is_empty());
        assert_eq!(config.len(), 1);
        assert_eq!(config.get_operation("population"), Some(AccumulatorOp::Sum));
    }

    #[test]
    fn test_config_get_operation_missing() {
        let config = AccumulatorConfig::new();
        assert_eq!(config.get_operation("nonexistent"), None);
    }

    #[test]
    fn test_config_multiple_operations() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("population", AccumulatorOp::Sum);
        config.set_operation("name", AccumulatorOp::Comma);
        config.set_operation("income", AccumulatorOp::Mean);

        assert_eq!(config.len(), 3);
        assert_eq!(config.get_operation("population"), Some(AccumulatorOp::Sum));
        assert_eq!(config.get_operation("name"), Some(AccumulatorOp::Comma));
        assert_eq!(config.get_operation("income"), Some(AccumulatorOp::Mean));
    }

    // ========================================================================
    // Sum Accumulator Tests
    // ========================================================================

    #[test]
    fn test_sum_int_int() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Sum);

        let mut target = HashMap::from([("value".to_string(), PropertyValue::Int(100))]);
        let source = HashMap::from([("value".to_string(), PropertyValue::Int(50))]);

        config.accumulate(&mut target, &source);

        assert_eq!(target.get("value"), Some(&PropertyValue::Int(150)));
    }

    #[test]
    fn test_sum_float_float() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Sum);

        let mut target = HashMap::from([("value".to_string(), PropertyValue::Float(1.5))]);
        let source = HashMap::from([("value".to_string(), PropertyValue::Float(2.5))]);

        config.accumulate(&mut target, &source);

        // 1.5 + 2.5 = 4.0 - whole numbers may be stored as Int for compactness
        match target.get("value") {
            Some(PropertyValue::Float(f)) => assert!((f - 4.0).abs() < 1e-10),
            Some(PropertyValue::Int(i)) => assert_eq!(*i, 4),
            _ => panic!("Expected numeric value"),
        }
    }

    #[test]
    fn test_sum_mixed_types() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Sum);

        let mut target = HashMap::from([("value".to_string(), PropertyValue::Int(100))]);
        let source = HashMap::from([("value".to_string(), PropertyValue::Float(0.5))]);

        config.accumulate(&mut target, &source);

        // Result should be Float since we have a fractional component
        match target.get("value") {
            Some(PropertyValue::Float(f)) => assert!((f - 100.5).abs() < 1e-10),
            _ => panic!("Expected Float"),
        }
    }

    #[test]
    fn test_sum_string_treated_as_zero() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Sum);

        let mut target = HashMap::from([("value".to_string(), PropertyValue::Int(100))]);
        let source = HashMap::from([(
            "value".to_string(),
            PropertyValue::String("not a number".to_string()),
        )]);

        config.accumulate(&mut target, &source);

        // String is treated as 0.0 for numeric ops (tippecanoe behavior)
        assert_eq!(target.get("value"), Some(&PropertyValue::Int(100)));
    }

    #[test]
    fn test_sum_missing_source_value() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Sum);

        let mut target = HashMap::from([("value".to_string(), PropertyValue::Int(100))]);
        let source = HashMap::new(); // No value for "value"

        config.accumulate(&mut target, &source);

        // Missing treated as 0
        assert_eq!(target.get("value"), Some(&PropertyValue::Int(100)));
    }

    #[test]
    fn test_sum_uint() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Sum);

        let mut target = HashMap::from([("value".to_string(), PropertyValue::UInt(100))]);
        let source = HashMap::from([("value".to_string(), PropertyValue::UInt(50))]);

        config.accumulate(&mut target, &source);

        assert_eq!(target.get("value"), Some(&PropertyValue::Int(150)));
    }

    // ========================================================================
    // Product Accumulator Tests
    // ========================================================================

    #[test]
    fn test_product_int_int() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Product);

        let mut target = HashMap::from([("value".to_string(), PropertyValue::Int(10))]);
        let source = HashMap::from([("value".to_string(), PropertyValue::Int(5))]);

        config.accumulate(&mut target, &source);

        assert_eq!(target.get("value"), Some(&PropertyValue::Int(50)));
    }

    #[test]
    fn test_product_float_float() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Product);

        let mut target = HashMap::from([("value".to_string(), PropertyValue::Float(2.5))]);
        let source = HashMap::from([("value".to_string(), PropertyValue::Float(4.0))]);

        config.accumulate(&mut target, &source);

        match target.get("value") {
            Some(PropertyValue::Float(f)) => assert!((f - 10.0).abs() < 1e-10),
            Some(PropertyValue::Int(i)) => assert_eq!(*i, 10),
            _ => panic!("Expected numeric"),
        }
    }

    #[test]
    fn test_product_with_zero() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Product);

        let mut target = HashMap::from([("value".to_string(), PropertyValue::Int(100))]);
        let source = HashMap::from([("value".to_string(), PropertyValue::Int(0))]);

        config.accumulate(&mut target, &source);

        assert_eq!(target.get("value"), Some(&PropertyValue::Int(0)));
    }

    #[test]
    fn test_product_string_treated_as_zero() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Product);

        let mut target = HashMap::from([("value".to_string(), PropertyValue::Int(100))]);
        let source = HashMap::from([(
            "value".to_string(),
            PropertyValue::String("text".to_string()),
        )]);

        config.accumulate(&mut target, &source);

        // String is treated as 0.0 for numeric ops
        assert_eq!(target.get("value"), Some(&PropertyValue::Int(0)));
    }

    // ========================================================================
    // Mean Accumulator Tests (CRITICAL - count tracking)
    // ========================================================================

    #[test]
    fn test_mean_two_values() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Mean);

        let mut target = HashMap::from([("value".to_string(), PropertyValue::Float(10.0))]);
        let source = HashMap::from([("value".to_string(), PropertyValue::Float(20.0))]);

        config.accumulate(&mut target, &source);

        // Mean of 10 and 20 = 15
        match target.get("value") {
            Some(PropertyValue::Float(f)) => assert!((f - 15.0).abs() < 1e-10),
            _ => panic!("Expected Float"),
        }
    }

    #[test]
    fn test_mean_three_values_running() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Mean);

        let mut target = HashMap::from([("value".to_string(), PropertyValue::Float(10.0))]);

        // Accumulate second value
        let source1 = HashMap::from([("value".to_string(), PropertyValue::Float(20.0))]);
        config.accumulate(&mut target, &source1);

        // Mean should be 15 after two values
        match target.get("value") {
            Some(PropertyValue::Float(f)) => assert!((f - 15.0).abs() < 1e-10),
            _ => panic!("Expected Float"),
        }

        // Accumulate third value
        let source2 = HashMap::from([("value".to_string(), PropertyValue::Float(30.0))]);
        config.accumulate(&mut target, &source2);

        // Mean of 10, 20, 30 = 20
        match target.get("value") {
            Some(PropertyValue::Float(f)) => assert!((f - 20.0).abs() < 1e-10),
            _ => panic!("Expected Float"),
        }
    }

    #[test]
    fn test_mean_with_integers() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Mean);

        let mut target = HashMap::from([("value".to_string(), PropertyValue::Int(10))]);
        let source = HashMap::from([("value".to_string(), PropertyValue::Int(20))]);

        config.accumulate(&mut target, &source);

        // Mean is always stored as Float for precision
        match target.get("value") {
            Some(PropertyValue::Float(f)) => assert!((f - 15.0).abs() < 1e-10),
            _ => panic!("Expected Float"),
        }
    }

    #[test]
    fn test_mean_reset_counts() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Mean);

        // First sequence
        let mut target1 = HashMap::from([("value".to_string(), PropertyValue::Float(10.0))]);
        let source1 = HashMap::from([("value".to_string(), PropertyValue::Float(20.0))]);
        config.accumulate(&mut target1, &source1);

        // Reset counts for new sequence
        config.reset_counts();

        // Second sequence should start fresh
        let mut target2 = HashMap::from([("value".to_string(), PropertyValue::Float(100.0))]);
        let source2 = HashMap::from([("value".to_string(), PropertyValue::Float(200.0))]);
        config.accumulate(&mut target2, &source2);

        // Mean of 100, 200 = 150 (not affected by first sequence)
        match target2.get("value") {
            Some(PropertyValue::Float(f)) => assert!((f - 150.0).abs() < 1e-10),
            _ => panic!("Expected Float"),
        }
    }

    // ========================================================================
    // Max Accumulator Tests
    // ========================================================================

    #[test]
    fn test_max_int_int() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Max);

        let mut target = HashMap::from([("value".to_string(), PropertyValue::Int(100))]);
        let source = HashMap::from([("value".to_string(), PropertyValue::Int(150))]);

        config.accumulate(&mut target, &source);

        assert_eq!(target.get("value"), Some(&PropertyValue::Int(150)));
    }

    #[test]
    fn test_max_target_larger() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Max);

        let mut target = HashMap::from([("value".to_string(), PropertyValue::Int(200))]);
        let source = HashMap::from([("value".to_string(), PropertyValue::Int(100))]);

        config.accumulate(&mut target, &source);

        assert_eq!(target.get("value"), Some(&PropertyValue::Int(200)));
    }

    #[test]
    fn test_max_negative_numbers() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Max);

        let mut target = HashMap::from([("value".to_string(), PropertyValue::Int(-100))]);
        let source = HashMap::from([("value".to_string(), PropertyValue::Int(-50))]);

        config.accumulate(&mut target, &source);

        assert_eq!(target.get("value"), Some(&PropertyValue::Int(-50)));
    }

    #[test]
    fn test_max_float() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Max);

        let mut target = HashMap::from([("value".to_string(), PropertyValue::Float(1.5))]);
        let source = HashMap::from([("value".to_string(), PropertyValue::Float(2.5))]);

        config.accumulate(&mut target, &source);

        match target.get("value") {
            Some(PropertyValue::Float(f)) => assert!((f - 2.5).abs() < 1e-10),
            _ => panic!("Expected Float"),
        }
    }

    #[test]
    fn test_max_skips_string() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Max);

        let mut target = HashMap::from([("value".to_string(), PropertyValue::Int(100))]);
        let source = HashMap::from([(
            "value".to_string(),
            PropertyValue::String("not a number".to_string()),
        )]);

        config.accumulate(&mut target, &source);

        // String is skipped, target value preserved
        assert_eq!(target.get("value"), Some(&PropertyValue::Int(100)));
    }

    // ========================================================================
    // Min Accumulator Tests
    // ========================================================================

    #[test]
    fn test_min_int_int() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Min);

        let mut target = HashMap::from([("value".to_string(), PropertyValue::Int(100))]);
        let source = HashMap::from([("value".to_string(), PropertyValue::Int(50))]);

        config.accumulate(&mut target, &source);

        assert_eq!(target.get("value"), Some(&PropertyValue::Int(50)));
    }

    #[test]
    fn test_min_target_smaller() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Min);

        let mut target = HashMap::from([("value".to_string(), PropertyValue::Int(50))]);
        let source = HashMap::from([("value".to_string(), PropertyValue::Int(100))]);

        config.accumulate(&mut target, &source);

        assert_eq!(target.get("value"), Some(&PropertyValue::Int(50)));
    }

    #[test]
    fn test_min_negative_numbers() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Min);

        let mut target = HashMap::from([("value".to_string(), PropertyValue::Int(-50))]);
        let source = HashMap::from([("value".to_string(), PropertyValue::Int(-100))]);

        config.accumulate(&mut target, &source);

        assert_eq!(target.get("value"), Some(&PropertyValue::Int(-100)));
    }

    #[test]
    fn test_min_skips_string() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Min);

        let mut target = HashMap::from([("value".to_string(), PropertyValue::Int(100))]);
        let source = HashMap::from([(
            "value".to_string(),
            PropertyValue::String("not a number".to_string()),
        )]);

        config.accumulate(&mut target, &source);

        // String is skipped, target value preserved
        assert_eq!(target.get("value"), Some(&PropertyValue::Int(100)));
    }

    // ========================================================================
    // Concat Accumulator Tests
    // ========================================================================

    #[test]
    fn test_concat_strings() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Concat);

        let mut target = HashMap::from([(
            "value".to_string(),
            PropertyValue::String("Hello".to_string()),
        )]);
        let source = HashMap::from([(
            "value".to_string(),
            PropertyValue::String("World".to_string()),
        )]);

        config.accumulate(&mut target, &source);

        assert_eq!(
            target.get("value"),
            Some(&PropertyValue::String("HelloWorld".to_string()))
        );
    }

    #[test]
    fn test_concat_empty_strings() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Concat);

        let mut target =
            HashMap::from([("value".to_string(), PropertyValue::String(String::new()))]);
        let source = HashMap::from([(
            "value".to_string(),
            PropertyValue::String("World".to_string()),
        )]);

        config.accumulate(&mut target, &source);

        assert_eq!(
            target.get("value"),
            Some(&PropertyValue::String("World".to_string()))
        );
    }

    #[test]
    fn test_concat_numbers_to_strings() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Concat);

        let mut target = HashMap::from([("value".to_string(), PropertyValue::Int(42))]);
        let source = HashMap::from([("value".to_string(), PropertyValue::Int(24))]);

        config.accumulate(&mut target, &source);

        // Numbers are converted to strings
        assert_eq!(
            target.get("value"),
            Some(&PropertyValue::String("4224".to_string()))
        );
    }

    // ========================================================================
    // Comma Accumulator Tests
    // ========================================================================

    #[test]
    fn test_comma_strings() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Comma);

        let mut target = HashMap::from([(
            "value".to_string(),
            PropertyValue::String("Alice".to_string()),
        )]);
        let source = HashMap::from([(
            "value".to_string(),
            PropertyValue::String("Bob".to_string()),
        )]);

        config.accumulate(&mut target, &source);

        assert_eq!(
            target.get("value"),
            Some(&PropertyValue::String("Alice,Bob".to_string()))
        );
    }

    #[test]
    fn test_comma_multiple() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Comma);

        let mut target =
            HashMap::from([("value".to_string(), PropertyValue::String("A".to_string()))]);

        config.accumulate(
            &mut target,
            &HashMap::from([("value".to_string(), PropertyValue::String("B".to_string()))]),
        );
        config.accumulate(
            &mut target,
            &HashMap::from([("value".to_string(), PropertyValue::String("C".to_string()))]),
        );

        assert_eq!(
            target.get("value"),
            Some(&PropertyValue::String("A,B,C".to_string()))
        );
    }

    #[test]
    fn test_comma_empty_target() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Comma);

        let mut target =
            HashMap::from([("value".to_string(), PropertyValue::String(String::new()))]);
        let source = HashMap::from([(
            "value".to_string(),
            PropertyValue::String("Bob".to_string()),
        )]);

        config.accumulate(&mut target, &source);

        // No leading comma when target is empty
        assert_eq!(
            target.get("value"),
            Some(&PropertyValue::String("Bob".to_string()))
        );
    }

    #[test]
    fn test_comma_empty_source() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Comma);

        let mut target = HashMap::from([(
            "value".to_string(),
            PropertyValue::String("Alice".to_string()),
        )]);
        let source = HashMap::from([("value".to_string(), PropertyValue::String(String::new()))]);

        config.accumulate(&mut target, &source);

        // No trailing comma when source is empty
        assert_eq!(
            target.get("value"),
            Some(&PropertyValue::String("Alice".to_string()))
        );
    }

    // ========================================================================
    // Count Accumulator Tests
    // ========================================================================

    #[test]
    fn test_count_from_zero() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("count", AccumulatorOp::Count);

        let mut target = HashMap::new();
        let source = HashMap::new();

        config.accumulate(&mut target, &source);

        assert_eq!(target.get("count"), Some(&PropertyValue::Int(1)));
    }

    #[test]
    fn test_count_increment() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("count", AccumulatorOp::Count);

        let mut target = HashMap::from([("count".to_string(), PropertyValue::Int(5))]);
        let source = HashMap::new();

        config.accumulate(&mut target, &source);

        assert_eq!(target.get("count"), Some(&PropertyValue::Int(6)));
    }

    #[test]
    fn test_count_multiple_increments() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("count", AccumulatorOp::Count);

        let mut target = HashMap::from([("count".to_string(), PropertyValue::Int(0))]);
        let empty = HashMap::new();

        for _ in 0..10 {
            config.accumulate(&mut target, &empty);
        }

        assert_eq!(target.get("count"), Some(&PropertyValue::Int(10)));
    }

    // ========================================================================
    // Unspecified Attributes Test (CRITICAL - tippecanoe behavior)
    // ========================================================================

    #[test]
    fn test_unspecified_attributes_dropped() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("keep_me", AccumulatorOp::Sum);

        let mut target = HashMap::from([
            ("keep_me".to_string(), PropertyValue::Int(100)),
            ("drop_me".to_string(), PropertyValue::Int(200)),
            (
                "also_drop_me".to_string(),
                PropertyValue::String("bye".to_string()),
            ),
        ]);

        let source = HashMap::from([
            ("keep_me".to_string(), PropertyValue::Int(50)),
            ("drop_me".to_string(), PropertyValue::Int(300)),
        ]);

        config.accumulate(&mut target, &source);

        // Only "keep_me" should remain
        assert_eq!(target.len(), 1);
        assert_eq!(target.get("keep_me"), Some(&PropertyValue::Int(150)));
        assert!(!target.contains_key("drop_me"));
        assert!(!target.contains_key("also_drop_me"));
    }

    // ========================================================================
    // Multiple Operations Test
    // ========================================================================

    #[test]
    fn test_multiple_operations_together() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("population", AccumulatorOp::Sum);
        config.set_operation("names", AccumulatorOp::Comma);
        config.set_operation("max_height", AccumulatorOp::Max);
        config.set_operation("feature_count", AccumulatorOp::Count);

        let mut target = HashMap::from([
            ("population".to_string(), PropertyValue::Int(100)),
            (
                "names".to_string(),
                PropertyValue::String("Alice".to_string()),
            ),
            ("max_height".to_string(), PropertyValue::Float(10.5)),
            ("feature_count".to_string(), PropertyValue::Int(1)),
        ]);

        let source = HashMap::from([
            ("population".to_string(), PropertyValue::Int(50)),
            (
                "names".to_string(),
                PropertyValue::String("Bob".to_string()),
            ),
            ("max_height".to_string(), PropertyValue::Float(15.5)),
        ]);

        config.accumulate(&mut target, &source);

        assert_eq!(target.get("population"), Some(&PropertyValue::Int(150)));
        assert_eq!(
            target.get("names"),
            Some(&PropertyValue::String("Alice,Bob".to_string()))
        );
        match target.get("max_height") {
            Some(PropertyValue::Float(f)) => assert!((f - 15.5).abs() < 1e-10),
            _ => panic!("Expected Float"),
        }
        assert_eq!(target.get("feature_count"), Some(&PropertyValue::Int(2)));
    }

    // ========================================================================
    // Type Coercion Edge Cases
    // ========================================================================

    #[test]
    fn test_bool_to_numeric() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Sum);

        let mut target = HashMap::from([("value".to_string(), PropertyValue::Bool(true))]);
        let source = HashMap::from([("value".to_string(), PropertyValue::Bool(true))]);

        config.accumulate(&mut target, &source);

        // true = 1.0, so 1 + 1 = 2
        assert_eq!(target.get("value"), Some(&PropertyValue::Int(2)));
    }

    #[test]
    fn test_bool_to_string() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Comma);

        let mut target = HashMap::from([("value".to_string(), PropertyValue::Bool(true))]);
        let source = HashMap::from([("value".to_string(), PropertyValue::Bool(false))]);

        config.accumulate(&mut target, &source);

        assert_eq!(
            target.get("value"),
            Some(&PropertyValue::String("true,false".to_string()))
        );
    }

    // ========================================================================
    // Edge Cases
    // ========================================================================

    #[test]
    fn test_empty_config_clears_target() {
        let mut config = AccumulatorConfig::new();
        // No operations configured

        let mut target = HashMap::from([
            ("a".to_string(), PropertyValue::Int(1)),
            ("b".to_string(), PropertyValue::Int(2)),
        ]);

        config.accumulate(&mut target, &HashMap::new());

        // All attributes dropped since none are configured
        assert!(target.is_empty());
    }

    #[test]
    fn test_empty_target() {
        let mut config = AccumulatorConfig::new();
        config.set_operation("value", AccumulatorOp::Sum);

        let mut target = HashMap::new();
        let source = HashMap::from([("value".to_string(), PropertyValue::Int(50))]);

        config.accumulate(&mut target, &source);

        // 0 + 50 = 50
        assert_eq!(target.get("value"), Some(&PropertyValue::Int(50)));
    }
}
