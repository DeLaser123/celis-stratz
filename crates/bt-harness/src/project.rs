//! Folder-native project contract.
//!
//! A project IS a folder. Discovery is deterministic and documented:
//!
//! ```text
//! <project>/
//!   Strategy/v<N|X.Y.Z>/   strategy.md (source) + strategy.yaml (compiled)
//!   Data/                  *.csv chart data + dataset.md notes
//!   Trades/                signals_*.csv, history_*.csv
//!   Notes/                 any *.md (AI context, playbooks)
//!   .celis/                harness state (registry, results, cache, config)
//! ```
//!
//! The harness discovers all of it; the kernel never touches folders.

use bt_core::error::{CoreError, CoreResult};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const STRATEGY_DIR: &str = "Strategy";
pub const DATA_DIR: &str = "Data";
pub const TRADES_DIR: &str = "Trades";
pub const NOTES_DIR: &str = "Notes";
pub const STATE_DIR: &str = ".stratz";
pub const RESULTS_DIR: &str = "results";
pub const CACHE_DIR: &str = "cache";
pub const AI_DIR: &str = "ai";
pub const REGISTRY_DB: &str = "registry.db";
pub const CONFIG_FILE: &str = "config.toml";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct StrategyVersion {
    /// Directory name, e.g. "v2.1.0".
    pub name: String,
    /// Parsed (major, minor, patch); "v3" == (3, 0, 0).
    pub semver: (u64, u64, u64),
    pub path: PathBuf,
    pub has_markdown: bool,
    pub has_spec: bool,
    pub has_config: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DataFile {
    pub path: PathBuf,
    pub symbols: Vec<String>,
    /// Inferred from the filename suffix `_1h.csv` style, else from the data.
    pub timeframe_hint: Option<String>,
    pub rows: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorCheck {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Ok,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub root: PathBuf,
    pub checks: Vec<DoctorCheck>,
    pub strategy_version: Option<String>,
    pub data_files: usize,
    pub signal_files: usize,
    pub notes: usize,
}

impl DoctorReport {
    pub fn is_healthy(&self) -> bool {
        self.checks.iter().all(|c| c.status != CheckStatus::Error)
    }
}

#[derive(Clone)]
pub struct Project {
    pub root: PathBuf,
}

impl Project {
    /// The project root for the current working directory: walks up until a
    /// folder containing `.celis` or `Strategy` is found.
    pub fn discover(start: &Path) -> CoreResult<Project> {
        let mut cur = Some(start);
        while let Some(dir) = cur {
            if dir.join(STATE_DIR).is_dir() || dir.join(STRATEGY_DIR).is_dir() {
                return Ok(Project {
                    root: dir.to_path_buf(),
                });
            }
            cur = dir.parent();
        }
        Err(CoreError::InvalidData(format!(
            "not inside a stratz project (no '{STATE_DIR}' or '{STRATEGY_DIR}' folder found \
             walking up from {}); run `backtest init` to create one",
            start.display()
        )))
    }

    pub fn open_current() -> CoreResult<Project> {
        let cwd = std::env::current_dir().map_err(CoreError::Io)?;
        Project::discover(&cwd)
    }

    /// Scaffold a new project in `dir`.
    pub fn init(dir: &Path) -> CoreResult<Project> {
        std::fs::create_dir_all(dir)?;
        for sub in [STRATEGY_DIR, DATA_DIR, TRADES_DIR, NOTES_DIR] {
            std::fs::create_dir_all(dir.join(sub))?;
        }
        let state = dir.join(STATE_DIR);
        std::fs::create_dir_all(state.join(RESULTS_DIR))?;
        std::fs::create_dir_all(state.join(CACHE_DIR))?;
        std::fs::create_dir_all(state.join(AI_DIR))?;

        let v1 = dir.join(STRATEGY_DIR).join("v1");
        std::fs::create_dir_all(&v1)?;
        let md = v1.join("strategy.md");
        if !md.exists() {
            std::fs::write(&md, STRATEGY_TEMPLATE)?;
        }
        let dataset_md = dir.join(DATA_DIR).join("dataset.md");
        if !dataset_md.exists() {
            std::fs::write(&dataset_md, DATASET_TEMPLATE)?;
        }
        let cfg = dir.join(STATE_DIR).join(CONFIG_FILE);
        if !cfg.exists() {
            std::fs::write(&cfg, HARNESS_CONFIG_TEMPLATE)?;
        }
        Ok(Project {
            root: dir.to_path_buf(),
        })
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.join(STATE_DIR)
    }
    pub fn results_dir(&self) -> PathBuf {
        self.state_dir().join(RESULTS_DIR)
    }
    pub fn registry_path(&self) -> PathBuf {
        self.state_dir().join(REGISTRY_DB)
    }
    pub fn config_path(&self) -> PathBuf {
        self.state_dir().join(CONFIG_FILE)
    }

    /// Strategy versions, sorted DESCENDING (latest first).
    pub fn strategy_versions(&self) -> CoreResult<Vec<StrategyVersion>> {
        let dir = self.root.join(STRATEGY_DIR);
        let mut out = Vec::new();
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| CoreError::InvalidData(format!("read {}: {e}", dir.display())))?;
        for entry in entries {
            let entry = entry.map_err(CoreError::Io)?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            let Some(semver) = parse_version_name(&name) else {
                continue; // not a version folder — ignored, never guessed
            };
            out.push(StrategyVersion {
                has_markdown: path.join("strategy.md").exists(),
                has_spec: path.join("strategy.yaml").exists(),
                has_config: path.join("config.yaml").exists(),
                semver,
                name,
                path,
            });
        }
        out.sort_by(|a, b| b.semver.cmp(&a.semver));
        Ok(out)
    }

    /// Resolve the strategy to use: explicit version name, else latest.
    pub fn resolve_strategy(&self, version: Option<&str>) -> CoreResult<StrategyVersion> {
        let versions = self.strategy_versions()?;
        if versions.is_empty() {
            return Err(CoreError::InvalidData(format!(
                "no strategy versions found under {}/{STRATEGY_DIR}",
                self.root.display()
            )));
        }
        match version {
            None => Ok(versions[0].clone()),
            Some(v) => versions
                .iter()
                .find(|s| s.name == v)
                .cloned()
                .ok_or_else(|| {
                    CoreError::InvalidData(format!(
                        "strategy version '{v}' not found; available: {}",
                        versions
                            .iter()
                            .map(|s| s.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                }),
        }
    }

    /// Chart/market data files under Data/ (header-sniffed).
    pub fn data_files(&self) -> CoreResult<Vec<DataFile>> {
        list_csv_with_sniff(&self.root.join(DATA_DIR), false)
    }

    /// External signal files: Trades/signals_*.csv.
    pub fn signal_files(&self) -> CoreResult<Vec<PathBuf>> {
        let dir = self.root.join(TRADES_DIR);
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.starts_with("signals_") && name.ends_with(".csv") {
                    out.push(path);
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// Markdown context files: Notes/*.md + Data/dataset.md.
    pub fn note_files(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(self.root.join(NOTES_DIR)) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("md") {
                    out.push(p);
                }
            }
        }
        let ds = self.root.join(DATA_DIR).join("dataset.md");
        if ds.exists() {
            out.push(ds);
        }
        out.sort();
        out
    }

    /// Full health check of the folder contract.
    pub fn doctor(&self) -> CoreResult<DoctorReport> {
        let mut checks = Vec::new();
        let mut data_files = 0usize;
        let mut signal_files = 0usize;
        let notes = self.note_files().len();
        let mut strategy_version = None;

        // Strategy
        match self.strategy_versions() {
            Ok(vs) if !vs.is_empty() => {
                let latest = &vs[0];
                strategy_version = Some(latest.name.clone());
                if latest.has_spec {
                    checks.push(DoctorCheck {
                        name: "strategy".into(),
                        status: CheckStatus::Ok,
                        detail: format!(
                            "{} version(s); latest '{}' has a compiled strategy.yaml",
                            vs.len(),
                            latest.name
                        ),
                    });
                } else if latest.has_markdown {
                    checks.push(DoctorCheck {
                        name: "strategy".into(),
                        status: CheckStatus::Warn,
                        detail: format!(
                            "latest '{}' has strategy.md but no compiled strategy.yaml \
                             (run `backtest ai compile` or write it manually)",
                            latest.name
                        ),
                    });
                } else {
                    checks.push(DoctorCheck {
                        name: "strategy".into(),
                        status: CheckStatus::Error,
                        detail: format!(
                            "latest '{}' has neither strategy.md nor strategy.yaml",
                            latest.name
                        ),
                    });
                }
            }
            Ok(_) => checks.push(DoctorCheck {
                name: "strategy".into(),
                status: CheckStatus::Error,
                detail: "Strategy/ folder contains no version folders (v1, v2, ...)".into(),
            }),
            Err(e) => checks.push(DoctorCheck {
                name: "strategy".into(),
                status: CheckStatus::Error,
                detail: e.to_string(),
            }),
        }

        // Data
        match self.data_files() {
            Ok(files) => {
                data_files = files.len();
                if files.is_empty() {
                    checks.push(DoctorCheck {
                        name: "data".into(),
                        status: CheckStatus::Error,
                        detail: "Data/ contains no CSV files with the OHLCV schema".into(),
                    });
                } else {
                    let syms: Vec<String> = files.iter().flat_map(|f| f.symbols.clone()).collect();
                    checks.push(DoctorCheck {
                        name: "data".into(),
                        status: CheckStatus::Ok,
                        detail: format!(
                            "{data_files} dataset file(s), symbols: {}",
                            syms.join(", ")
                        ),
                    });
                }
            }
            Err(e) => checks.push(DoctorCheck {
                name: "data".into(),
                status: CheckStatus::Error,
                detail: e.to_string(),
            }),
        }

        // Signals
        match self.signal_files() {
            Ok(files) => {
                signal_files = files.len();
                checks.push(DoctorCheck {
                    name: "signals".into(),
                    status: CheckStatus::Ok,
                    detail: format!("{signal_files} signals_*.csv file(s) in Trades/"),
                });
            }
            Err(e) => checks.push(DoctorCheck {
                name: "signals".into(),
                status: CheckStatus::Warn,
                detail: e.to_string(),
            }),
        }

        // State
        let state_ok = self.state_dir().is_dir();
        checks.push(DoctorCheck {
            name: "state".into(),
            status: if state_ok {
                CheckStatus::Ok
            } else {
                CheckStatus::Warn
            },
            detail: if state_ok {
                format!(
                    ".celis/ present (registry at {})",
                    self.registry_path().display()
                )
            } else {
                ".celis/ missing (created automatically on first run)".into()
            },
        });

        // Notes
        checks.push(DoctorCheck {
            name: "notes".into(),
            status: if notes > 0 {
                CheckStatus::Ok
            } else {
                CheckStatus::Warn
            },
            detail: format!("{notes} markdown context file(s) for AI grounding"),
        });

        let report = DoctorReport {
            root: self.root.clone(),
            checks,
            strategy_version,
            data_files,
            signal_files,
            notes,
        };
        Ok(report)
    }
}

/// "v3" -> (3,0,0); "v2.1" -> (2,1,0); "v2.1.0" -> (2,1,0). Returns None for
/// anything else (never guessed).
pub fn parse_version_name(name: &str) -> Option<(u64, u64, u64)> {
    let rest = name.strip_prefix('v')?;
    let mut parts = rest.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map(|s| s.parse().unwrap_or(0)).unwrap_or(0);
    let patch = parts.next().map(|s| s.parse().unwrap_or(0)).unwrap_or(0);
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Sniff CSV files: count data rows and extract symbols from the symbol
/// column without a full validation load.
fn list_csv_with_sniff(dir: &Path, _recursive: bool) -> CoreResult<Vec<DataFile>> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(CoreError::Io)?
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.ends_with(".csv") || name.ends_with(".md") {
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(CoreError::Io)?;
        let mut reader = csv::ReaderBuilder::new().from_reader(text.as_bytes());
        let headers = reader.headers().map_err(|e| {
            CoreError::InvalidData(format!("{}: cannot read header: {e}", path.display()))
        })?;
        let required = ["timestamp", "symbol", "open", "high", "low", "close"];
        if !required
            .iter()
            .all(|r| headers.iter().any(|h| h.trim() == *r))
        {
            continue; // not OHLCV data — skipped silently from data listing
        }
        let sym_idx = headers.iter().position(|h| h.trim() == "symbol");
        let mut symbols = Vec::new();
        let mut rows = 0usize;
        for rec in reader.records().flatten() {
            rows += 1;
            if let Some(i) = sym_idx {
                let s = rec.get(i).unwrap_or("").trim().to_string();
                if !s.is_empty() && !symbols.contains(&s) {
                    symbols.push(s);
                }
            }
            if rows >= 10_000 {
                break; // sniffing bound; full validation happens at load time
            }
        }
        let timeframe_hint = name
            .strip_suffix(".csv")
            .and_then(|stem| stem.rsplit('_').next())
            .map(|s| s.to_string())
            .filter(|s| bt_core::time::parse_interval(s).is_ok());
        out.push(DataFile {
            path,
            symbols,
            timeframe_hint,
            rows,
        });
    }
    Ok(out)
}

const STRATEGY_TEMPLATE: &str = r#"# Strategy: <name>

Describe the strategy in plain language. The AI compiler (`backtest ai compile`)
turns this document into `strategy.yaml`, which the engine can execute. The
engine rejects ambiguity, so state every rule precisely:

- Market and timeframe (e.g. EURUSD, 1h bars).
- Entry conditions (indicators, thresholds, crossings).
- Exit conditions (signals, stop-loss, take-profit, trailing).
- Position sizing and risk rules.
- Sessions/time filters and anything else that gates a trade.

Every assumption the compiler has to make is recorded in `assumptions.md`.
"#;

const DATASET_TEMPLATE: &str = r#"# Dataset notes

Describe the data in Data/: source, timezone, bar convention (open or close
timestamps), known gaps. This file is also provided to the AI as context.
"#;

const HARNESS_CONFIG_TEMPLATE: &str = r#"# Celis harness settings. The API key is NEVER stored here —
# provide it via --api-key, the CELIS_API_KEY environment variable,
# or the OS keyring (--save-key).

[ai]
provider = "zai"          # zai | openai-compatible
model = "glm-5.3-flash"
base_url = ""             # optional override (openai-compatible)
max_tokens = 4096
temperature = 0.0
max_repair_rounds = 3
budget_tokens_per_command = 200000

[runs]
default_starting_capital = "100000"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bt_harness_{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn init_scaffolds_contract() {
        let dir = tmpdir("init");
        let p = Project::init(&dir).unwrap();
        assert!(p.root.join("Strategy/v1/strategy.md").exists());
        assert!(p.root.join("Data").is_dir());
        assert!(p.root.join("Trades").is_dir());
        assert!(p.root.join("Notes").is_dir());
        assert!(p.state_dir().join("results").is_dir());
        assert!(p.config_path().exists());
        let report = p.doctor().unwrap();
        assert!(
            !report.is_healthy(),
            "fresh scaffold has no data — doctor must say so"
        );
        std::fs::write(
            dir.join("Data").join("X_1h.csv"),
            "timestamp,symbol,open,high,low,close,volume\n2024-01-01T00:00:00Z,X,1,1,1,1,1\n",
        )
        .unwrap();
        let report = p.doctor().unwrap();
        assert!(report.is_healthy(), "{report:?}");
    }

    #[test]
    fn version_sorting_is_semver_descending() {
        let dir = tmpdir("versions");
        Project::init(&dir).unwrap();
        for v in ["v1", "v2", "v2.1.0", "v10"] {
            std::fs::create_dir_all(dir.join("Strategy").join(v)).unwrap();
        }
        let p = Project { root: dir };
        let vs = p.strategy_versions().unwrap();
        let names: Vec<String> = vs.iter().map(|s| s.name.clone()).collect();
        assert_eq!(names, vec!["v10", "v2.1.0", "v2", "v1"]);
        assert_eq!(p.resolve_strategy(None).unwrap().name, "v10");
        assert_eq!(p.resolve_strategy(Some("v2")).unwrap().semver, (2, 0, 0));
        assert!(p.resolve_strategy(Some("v99")).is_err());
    }

    #[test]
    fn discovery_walks_up() {
        let dir = tmpdir("discover");
        Project::init(&dir).unwrap();
        let nested = dir.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        let p = Project::discover(&nested).unwrap();
        assert_eq!(p.root, dir);
        // outside a project: error, never guessed
        let outside = tmpdir("outside");
        assert!(Project::discover(&outside).is_err());
    }

    #[test]
    fn data_sniffing_and_doctor() {
        let dir = tmpdir("data");
        Project::init(&dir).unwrap();
        std::fs::write(
            dir.join("Data").join("EURUSD_1h.csv"),
            "timestamp,symbol,open,high,low,close,volume\n2024-01-01T00:00:00Z,EURUSD,1,1,1,1,1\n",
        )
        .unwrap();
        std::fs::write(dir.join("Data").join("readme.md"), "notes").unwrap();
        let p = Project { root: dir };
        let files = p.data_files().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].symbols, vec!["EURUSD"]);
        assert_eq!(files[0].timeframe_hint.as_deref(), Some("1h"));
        let report = p.doctor().unwrap();
        assert!(report.is_healthy());
        assert_eq!(report.strategy_version.as_deref(), Some("v1"));
    }
}
