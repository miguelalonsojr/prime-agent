use super::ClmError;
use pa_types::clm::ClmSnapshot;
use serde_json::Value;
use sha2::{Digest, Sha256};

// JS enumeration puts own integer-index keys before all other insertion-ordered keys.
fn array_index(key: &str) -> Option<u32> {
    let value = key.parse::<u32>().ok()?;
    (value < u32::MAX && value.to_string() == key).then_some(value)
}

pub(crate) fn js_stringify(value: &Value) -> Result<String, ClmError> {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort_by(|a, b| match (array_index(a), array_index(b)) {
                (Some(a), Some(b)) => a.cmp(&b),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            });
            let pairs = keys
                .into_iter()
                .map(|key| {
                    Ok(format!(
                        "{}:{}",
                        serde_json::to_string(key)
                            .map_err(|e| ClmError::document(e.to_string()))?,
                        js_stringify(&map[key])?
                    ))
                })
                .collect::<Result<Vec<_>, ClmError>>()?;
            Ok(format!("{{{}}}", pairs.join(",")))
        }
        Value::Array(items) => Ok(format!(
            "[{}]",
            items
                .iter()
                .map(js_stringify)
                .collect::<Result<Vec<_>, _>>()?
                .join(",")
        )),
        Value::Number(number) => {
            let n = number
                .as_f64()
                .ok_or_else(|| ClmError::document("non-finite number"))?;
            if n == 0.0 {
                return Ok("0".into());
            }
            let raw = serde_json::to_string(&n).map_err(|e| ClmError::document(e.to_string()))?;
            let (mantissa, exponent) = raw
                .split_once('e')
                .map_or((raw.as_str(), 0), |(base, exponent)| {
                    (base, exponent.parse::<i32>().unwrap_or_default())
                });
            if n.abs() >= 1e21 || n.abs() < 1e-6 {
                let base = mantissa.trim_end_matches(".0");
                return Ok(format!(
                    "{base}e{}{exponent}",
                    if exponent >= 0 { "+" } else { "" }
                ));
            }
            let mantissa = mantissa.trim_end_matches(".0");
            let negative = mantissa.starts_with('-');
            let digits = mantissa.trim_start_matches('-').replace('.', "");
            let decimal_at = mantissa
                .trim_start_matches('-')
                .find('.')
                .unwrap_or(mantissa.trim_start_matches('-').len())
                as i32
                + exponent;
            let sign = if negative { "-" } else { "" };
            let fixed = if decimal_at <= 0 {
                format!("{sign}0.{}{}", "0".repeat((-decimal_at) as usize), digits)
            } else if decimal_at as usize >= digits.len() {
                format!(
                    "{sign}{}{}",
                    digits,
                    "0".repeat(decimal_at as usize - digits.len())
                )
            } else {
                format!(
                    "{sign}{}.{}",
                    &digits[..decimal_at as usize],
                    &digits[decimal_at as usize..]
                )
            };
            Ok(fixed)
        }
        _ => serde_json::to_string(value).map_err(|e| ClmError::document(e.to_string())),
    }
}

/// Returns a CLM safety error for invalid or unavailable input.
///
/// # Errors
/// Returns an error if the document, selected ancestry or budget violates CLM policy.
pub fn snapshot_checksum(snapshot: &ClmSnapshot) -> Result<String, ClmError> {
    Ok(format!(
        "{:x}",
        Sha256::digest(js_stringify(snapshot.as_value())?.as_bytes())
    ))
}
