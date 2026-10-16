//! Money handling: decimal strings at the API boundary, integer micro-USD in
//! every ledger row (1 USD = 1_000_000 micro-USD).
use crate::error::{Error, Result};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use std::str::FromStr;

const MICROS_PER_USD: i64 = 1_000_000;
/// Upper bound for any single decimal input (prices, amounts, percentages).
const MAX_DECIMAL: u64 = 1_000_000_000;

/// Parses a non-negative decimal string no larger than one billion.
///
/// Scientific notation is accepted because JSON serializers emit small floats
/// that way (for example a provider-reported cost of `1.2e-5`).
pub fn decimal(s: &str) -> Result<Decimal> {
    let s = s.trim();
    let d = Decimal::from_str(s)
        .or_else(|_| Decimal::from_scientific(s))
        .map_err(|_| Error::bad("Expected a decimal string"))?;
    if d.is_sign_negative() || d > Decimal::from(MAX_DECIMAL) {
        return Err(Error::bad("Amount outside supported range"));
    }
    Ok(d)
}

/// Parses a USD decimal string into micro-USD.
pub fn usd(s: &str) -> Result<i64> {
    micro(decimal(s)?)
}

/// Converts a USD amount into micro-USD, rounding half away from zero.
pub fn micro(d: Decimal) -> Result<i64> {
    (d * Decimal::from(MICROS_PER_USD))
        .round()
        .to_i64()
        .ok_or_else(|| Error::bad("Amount overflow"))
}

/// Formats micro-USD as a normalized decimal string (`1500000` -> `"1.5"`).
pub fn display(micros: i64) -> String {
    (Decimal::from(micros) / Decimal::from(MICROS_PER_USD))
        .normalize()
        .to_string()
}

/// Applies a percentage markup string (`"30"` = +30%) to a micro-USD cost.
pub fn markup(micros: i64, pct: &str) -> Result<i64> {
    (Decimal::from(micros) * (Decimal::ONE + decimal(pct)? / Decimal::ONE_HUNDRED))
        .round()
        .to_i64()
        .ok_or_else(|| Error::bad("Billing overflow"))
}

/// Accepts a USD amount given as a JSON string or number.
pub fn value(v: &serde_json::Value) -> Result<i64> {
    usd(&decimal_text(v)?)
}

/// Accepts a percentage given as a JSON string or number and returns its
/// canonical string form, validated by [`decimal`].
pub fn percent(v: &serde_json::Value) -> Result<String> {
    let s = decimal_text(v)?;
    decimal(&s)?;
    Ok(s)
}

fn decimal_text(v: &serde_json::Value) -> Result<String> {
    match v {
        serde_json::Value::String(s) => Ok(s.trim().to_owned()),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        _ => Err(Error::bad("Expected a decimal string or number")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_numbers_in_scientific_notation_are_accepted() {
        // serde_json renders 0.000012 as "1.2e-5".
        assert_eq!(value(&json!(0.000012)).unwrap(), 12);
        assert_eq!(value(&json!("1.5")).unwrap(), 1_500_000);
        assert!(value(&json!(null)).is_err());
        assert!(value(&json!("-1")).is_err());
    }

    #[test]
    fn markup_rounds_to_whole_micros() {
        assert_eq!(markup(1_000, "30").unwrap(), 1_300);
        assert_eq!(markup(1, "50").unwrap(), 2);
        assert_eq!(display(1_500_000), "1.5");
    }
}
