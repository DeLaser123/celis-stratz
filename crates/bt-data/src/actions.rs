//! Corporate actions: cash dividends and splits, loaded from an optional CSV
//! side-file (`Data/corporate_actions.csv`).
//!
//! ```text
//! timestamp,symbol,kind,value
//! 2024-02-01T00:00:00Z,X,dividend,0.50
//! 2024-03-01T00:00:00Z,X,split,2.0
//! ```
//!
//! Conventions (documented in DATA_MODEL.md):
//! - `dividend`: cash amount per unit (quote currency) paid on the bar whose
//!   open_time == timestamp. Long positions receive; short positions pay.
//! - `split`: ratio = new units per old unit (2.0 = 2-for-1). Apply ONLY with
//!   unadjusted price data — the engine adjusts open positions at the
//!   ex-date bar; providing pre-adjusted prices double-counts the split.
//! - `value` is empty for splits that are encoded purely in prices? Never:
//!   the ratio is always required.

use bt_core::error::{CoreError, CoreResult};
use bt_core::time::{parse_timestamp, Ts};
use bt_core::D;
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CorporateActionKind {
    /// Cash dividend per unit, quote currency.
    Dividend { amount: D },
    /// Split ratio: new units per old unit (2.0 = 2-for-1).
    Split { ratio: D },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CorporateAction {
    pub ts: Ts,
    pub symbol: String,
    pub kind: CorporateActionKind,
}

/// Load corporate actions from CSV. Rows with unknown kinds or bad values are
/// hard errors (this file directly affects accounting).
pub fn load_corporate_actions_csv(
    path: &Path,
    tz: chrono_tz::Tz,
) -> CoreResult<Vec<CorporateAction>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| CoreError::InvalidData(format!("read {}: {e}", path.display())))?;
    let mut reader = csv::ReaderBuilder::new().from_reader(text.as_bytes());
    let headers = reader
        .headers()
        .map_err(|e| CoreError::InvalidData(format!("{}: {e}", path.display())))?
        .clone();
    let names: Vec<String> = headers.iter().map(|h| h.trim().to_string()).collect();
    let idx = |name: &str| names.iter().position(|h| h == name);
    let (Some(ts_i), Some(sym_i), Some(kind_i), Some(val_i)) =
        (idx("timestamp"), idx("symbol"), idx("kind"), idx("value"))
    else {
        return Err(CoreError::InvalidData(format!(
            "{}: corporate actions CSV requires columns timestamp,symbol,kind,value",
            path.display()
        )));
    };

    let mut out = Vec::new();
    for (ri, rec) in reader.records().enumerate() {
        let row = ri + 1;
        let rec: csv::StringRecord =
            rec.map_err(|e| CoreError::InvalidData(format!("{} row {row}: {e}", path.display())))?;
        let ts = parse_timestamp(
            rec.get(ts_i).unwrap_or(""),
            tz,
            &format!("{} row {row}", path.display()),
        )?;
        let symbol = rec.get(sym_i).unwrap_or("").trim().to_string();
        if symbol.is_empty() {
            return Err(CoreError::InvalidData(format!(
                "{} row {row}: empty symbol",
                path.display()
            )));
        }
        let kind_str = rec.get(kind_i).unwrap_or("").trim().to_lowercase();
        let value_raw = rec.get(val_i).unwrap_or("").trim();
        let value: D = value_raw.parse().map_err(|_| {
            CoreError::InvalidData(format!(
                "{} row {row}: value '{value_raw}' is not a decimal",
                path.display()
            ))
        })?;
        let kind = match kind_str.as_str() {
            "dividend" | "cash_dividend" => CorporateActionKind::Dividend { amount: value },
            "split" => {
                if value <= D::ZERO {
                    return Err(CoreError::InvalidData(format!(
                        "{} row {row}: split ratio must be positive",
                        path.display()
                    )));
                }
                CorporateActionKind::Split { ratio: value }
            }
            other => {
                return Err(CoreError::InvalidData(format!(
                    "{} row {row}: unknown corporate action kind '{other}' (dividend|split)",
                    path.display()
                )))
            }
        };
        out.push(CorporateAction { ts, symbol, kind });
    }
    // Deterministic order.
    out.sort_by(|a, b| (a.ts, &a.symbol).cmp(&(b.ts, &b.symbol)));
    Ok(out)
}

/// Apply all actions at `ts` for `symbol` to a bar's raw price level — used
/// by the engine to adjust open positions; the BAR itself is never mutated
/// here (that is the data file's job).
pub fn actions_at<'a>(
    actions: &'a [CorporateAction],
    ts: Ts,
    symbol: &str,
) -> Vec<&'a CorporateAction> {
    actions
        .iter()
        .filter(|a| a.ts == ts && a.symbol == symbol)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parses_and_sorts() {
        let dir = std::env::temp_dir().join(format!("bt_actions_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("actions.csv");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "timestamp,symbol,kind,value").unwrap();
        writeln!(f, "2024-03-01T00:00:00Z,X,split,2.0").unwrap();
        writeln!(f, "2024-02-01T00:00:00Z,X,dividend,0.50").unwrap();
        drop(f);
        let utc: chrono_tz::Tz = "UTC".parse().unwrap();
        let actions = load_corporate_actions_csv(&path, utc).unwrap();
        assert_eq!(actions.len(), 2);
        assert!(
            matches!(actions[0].kind, CorporateActionKind::Dividend { amount } if amount == D::from_str_exact("0.50").unwrap())
        );
        assert!(
            matches!(actions[1].kind, CorporateActionKind::Split { ratio } if ratio == D::from(2))
        );
        assert!(actions[0].ts < actions[1].ts, "sorted by ts");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_unknown_kind_and_bad_ratio() {
        let dir = std::env::temp_dir().join(format!("bt_actions_bad_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.csv");
        std::fs::write(
            &path,
            "timestamp,symbol,kind,value\n2024-01-01T00:00:00Z,X,merger,1.0\n",
        )
        .unwrap();
        let utc: chrono_tz::Tz = "UTC".parse().unwrap();
        assert!(load_corporate_actions_csv(&path, utc).is_err());
        std::fs::write(
            &path,
            "timestamp,symbol,kind,value\n2024-01-01T00:00:00Z,X,split,0\n",
        )
        .unwrap();
        assert!(load_corporate_actions_csv(&path, utc).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
