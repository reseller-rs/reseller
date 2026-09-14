//! Decimal arithmetic at the boundary; integer micro-USD in every ledger row.
use crate::error::{Error, Result};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use std::str::FromStr;
pub fn decimal(s: &str) -> Result<Decimal> {
    let d = Decimal::from_str(s).map_err(|_| Error::bad("Expected a decimal string"))?;
    if d < Decimal::ZERO || d > Decimal::from(1_000_000_000u64) {
        return Err(Error::bad("Amount outside supported range"));
    }
    Ok(d)
}
pub fn usd(s: &str) -> Result<i64> {
    micro(decimal(s)?)
}
pub fn micro(d: Decimal) -> Result<i64> {
    (d * Decimal::from(1_000_000))
        .round()
        .to_i64()
        .ok_or_else(|| Error::bad("Amount overflow"))
}
pub fn display(n: i64) -> String {
    (Decimal::from(n) / Decimal::from(1_000_000))
        .normalize()
        .to_string()
}
pub fn markup(n: i64, pct: &str) -> Result<i64> {
    (Decimal::from(n) * (Decimal::ONE + decimal(pct)? / Decimal::from(100)))
        .round()
        .to_i64()
        .ok_or_else(|| Error::bad("Billing overflow"))
}
pub fn value(v: &serde_json::Value) -> Result<i64> {
    usd(&v
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| v.to_string()))
}
