//! Dukascopy chart-data downloader (spec §5 data-source goal).
//!
//! Dukascopy publishes free historical candles as LZMA-compressed `.bi5`
//! files on `datafeed.dukascopy.com`. Layout (verified against the
//! dukascopy-node reference implementation):
//!
//! ```text
//! minute candles: {Y}/{M0}/{D:02d}/BID_candles_min_1.bi5   (one file per day)
//! hour candles:   {Y}/{M0}/{D:02d}/BID_candles_hour_1.bi5  (one file per day)
//! day candles:    {Y}/{M0}/BID_candles_day_1.bi5           (one file per month)
//! ```
//!
//! - `M0` is the ZERO-INDEXED month (January = 00) — a notorious quirk.
//! - Files are LZMA1 (alone format). Each decompressed record is 24 bytes,
//!   big-endian: `offset:u32, open:u32, close:u32, low:u32, high:u32,
//!   volume:f32` — note the O-C-L-H field order.
//! - `offset` is seconds from the file's period start (day start for
//!   per-day files, month start for per-month files).
//! - Prices are integers scaled by 10^decimals (5 for most FX, 3 for JPY
//!   pairs — configurable).
//! - Volume is Dukascopy's native unit (millions for FX), passed through.
//! - Empty days (market holidays) return HTTP 404 — skipped, never an error.
//!   Weekends DO serve files (verified against the live feed: FX candles are
//!   published for Saturdays/Sundays), so every calendar day is requested.
//!
//! Edge throttling: Dukascopy 503s clients that fetch too fast. Defenses:
//! one keep-alive agent per pull, 250 ms pacing between requests, a patient
//! exponential backoff honoring `Retry-After`, and a raw-file cache that
//! makes an interrupted pull resumable (`Bi5Cache`).
//!
//! Bars are fetched at the native granularity and resampled to the requested
//! timeframe with the engine's own resampler, so downloaded data and engine
//! expectations stay aligned by construction.

use crate::bar::{Bar, BarSeries};
use bt_core::error::{CoreError, CoreResult};
use bt_core::time::{format_ts, Ts};
use bt_core::D;
use chrono::Datelike;
use rust_decimal_macros::dec;
use std::io::Read;
use std::path::{Path, PathBuf};

const BASE: &str = "https://datafeed.dukascopy.com/datafeed";
const RECORD_LEN: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Bid,
    Ask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeSource {
    Min1,
    Day1,
}

/// Pull parameters for one symbol/timeframe/range.
#[derive(Debug, Clone)]
pub struct PullSpec {
    pub symbol: String,
    pub timeframe_secs: i64,
    pub from: Ts,
    pub to: Ts,
    pub side: Side,
    pub decimals: u32,
}

impl PullSpec {
    /// Default price decimals per instrument family (documented heuristic:
    /// JPY-quote FX pairs use 3, everything else 5 — override with
    /// `--decimals` for exotics/metals).
    pub fn default_decimals(symbol: &str) -> u32 {
        if symbol.to_uppercase().ends_with("JPY") {
            3
        } else {
            5
        }
    }
}

fn side_str(side: Side) -> &'static str {
    match side {
        Side::Bid => "BID",
        Side::Ask => "ASK",
    }
}

/// Native file granularity for a requested timeframe.
fn native_source(timeframe_secs: i64) -> (NativeSource, i64) {
    // Dukascopy serves only min_1 (per day) and day_1 (per month) candle
    // files — hour_1 files 404 (verified against the live feed). Everything
    // below 1d aggregates from min_1.
    if timeframe_secs >= 86_400 {
        (NativeSource::Day1, 86_400)
    } else {
        (NativeSource::Min1, 60)
    }
}

fn file_url(source: NativeSource, side: Side, symbol: &str, period_start: Ts) -> String {
    // Dukascopy's quirks: months are ZERO-INDEXED but ZERO-PADDED to two
    // digits (January = "00", December = "11"), days are 1-indexed padded.
    let (y, m0, d) = (
        period_start.year(),
        period_start.month0(), // zero-indexed
        period_start.day(),
    );
    let s = side_str(side);
    match source {
        NativeSource::Min1 => {
            format!("{BASE}/{symbol}/{y}/{m0:02}/{d:02}/{s}_candles_min_1.bi5")
        }
        NativeSource::Day1 => format!("{BASE}/{symbol}/{y}/{m0:02}/{s}_candles_day_1.bi5"),
    }
}

fn enumerate_files(source: NativeSource, spec: &PullSpec) -> Vec<(String, Ts)> {
    let mut out = Vec::new();
    match source {
        NativeSource::Min1 => {
            let mut day = spec.from.date_naive();
            let last = spec.to.date_naive();
            while day <= last {
                let start =
                    Ts::from_naive_utc_and_offset(day.and_hms_opt(0, 0, 0).unwrap(), chrono::Utc);
                out.push((file_url(source, spec.side, &spec.symbol, start), start));
                day += chrono::Duration::days(1);
            }
        }
        NativeSource::Day1 => {
            // Months intersecting [from, to]: linear index by year*12 + month0.
            let first = spec.from.year() * 12 + i32::try_from(spec.from.month0()).unwrap_or(0);
            let last = spec.to.year() * 12 + i32::try_from(spec.to.month0()).unwrap_or(0);
            let mut idx = first;
            while idx <= last {
                let year = idx / 12;
                let month0 = u32::try_from(idx % 12).unwrap_or(0);
                let start = Ts::from_naive_utc_and_offset(
                    chrono::NaiveDate::from_ymd_opt(year, month0 + 1, 1)
                        .unwrap()
                        .and_hms_opt(0, 0, 0)
                        .unwrap(),
                    chrono::Utc,
                );
                out.push((file_url(source, spec.side, &spec.symbol, start), start));
                idx += 1;
            }
        }
    }
    out
}

/// Dukascopy's edge requires browser-like headers — plain UAs get 503-blocked.
fn browser_headers(req: ureq::Request) -> ureq::Request {
    req.set(
        "User-Agent",
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36",
    )
    .set("Referer", "https://www.dukascopy.com/trading-tools/")
    .set("Accept", "*/*")
    .set("Accept-Language", "en-US,en;q=0.9")
    .set("Sec-Fetch-Dest", "empty")
    .set("Sec-Fetch-Mode", "cors")
    .set("Sec-Fetch-Site", "same-site")
}

const WARMUP_URL: &str = "https://www.dukascopy.com/trading-tools/";

/// The datafeed is normally fetched by a browser that first loaded the
/// referring page, so do the same once per pull: it establishes session
/// cookies on the shared agent. Best-effort — never fails the pull.
fn warm_up(agent: &ureq::Agent) {
    let _ = browser_headers(agent.get(WARMUP_URL))
        .timeout(std::time::Duration::from_secs(5))
        .call();
}

/// One shared agent per pull: keep-alive avoids the fresh-TLS-per-request
/// pattern that the edge treats as bot traffic.
fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new().build()
}

/// Backoff before each 503 retry (ms). The edge's throttle cool-downs outlast
/// short retries, so patience beats hammering: ~3 minutes cumulative.
const RETRY_DELAYS_MS: [u64; 8] = [1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 60_000, 60_000];
const MAX_RETRY_AFTER_MS: u64 = 120_000;

fn fetch_bi5(agent: &ureq::Agent, url: &str) -> CoreResult<Option<Vec<u8>>> {
    let last = RETRY_DELAYS_MS.len();
    // `scheduled_ms` is the wait after attempt N fails (attempt 8 is the
    // final one and errors out — its slot is never slept).
    let schedule = RETRY_DELAYS_MS.iter().copied().chain(std::iter::once(0));
    for (attempt, scheduled_ms) in schedule.enumerate() {
        match browser_headers(agent.get(url))
            .timeout(std::time::Duration::from_secs(30))
            .call()
        {
            Ok(resp) => {
                let mut buf = Vec::new();
                resp.into_reader()
                    .read_to_end(&mut buf)
                    .map_err(|e| CoreError::InvalidData(format!("read {url}: {e}")))?;
                return Ok(Some(buf));
            }
            Err(ureq::Error::Status(404, _)) => return Ok(None),
            Err(ureq::Error::Status(503, resp)) => {
                let retry_after_ms = resp
                    .header("retry-after")
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .map(|s| s.saturating_mul(1_000).min(MAX_RETRY_AFTER_MS));
                if attempt < last {
                    let delay_ms = retry_after_ms.unwrap_or(scheduled_ms);
                    eprintln!(
                        "  throttled (503), waiting {}s (retry {} of {last})",
                        delay_ms / 1_000,
                        attempt + 1
                    );
                    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                    continue;
                }
                return Err(CoreError::InvalidData(format!(
                    "HTTP 503 from {url} (Dukascopy edge throttling; retries exhausted)"
                )));
            }
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                if attempt < last {
                    eprintln!("  HTTP {code}, retrying ({attempt} of {last})");
                    std::thread::sleep(std::time::Duration::from_millis(scheduled_ms.max(1_000)));
                    continue;
                }
                return Err(CoreError::InvalidData(format!(
                    "HTTP {code} from {url}: {}",
                    body.chars().take(200).collect::<String>()
                )));
            }
            Err(e) => {
                if attempt < last {
                    eprintln!("  network error ({e}), retrying ({attempt} of {last})");
                    std::thread::sleep(std::time::Duration::from_millis(scheduled_ms.max(1_000)));
                    continue;
                }
                return Err(CoreError::InvalidData(format!("fetch {url}: {e}")));
            }
        }
    }
    unreachable!()
}

/// Raw `.bi5` download cache, keyed by URL hash. Closed periods are immutable
/// history on Dukascopy, so entries never need invalidation — this is what
/// makes an interrupted pull resumable. The still-growing current period is
/// never written (see `pull_with`), so range ends stay accurate.
pub struct Bi5Cache {
    dir: PathBuf,
}

impl Bi5Cache {
    pub fn new(dir: impl Into<PathBuf>) -> CoreResult<Bi5Cache> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(CoreError::Io)?;
        Ok(Bi5Cache { dir })
    }

    fn path(&self, url: &str) -> PathBuf {
        self.dir
            .join(format!("{}.bi5", bt_core::hash::sha256_hex(url.as_bytes())))
    }

    pub fn get(&self, url: &str) -> Option<Vec<u8>> {
        std::fs::read(self.path(url)).ok()
    }

    /// Best-effort: a cache write failure must never fail a pull.
    pub fn put(&self, url: &str, bytes: &[u8]) {
        let _ = std::fs::write(self.path(url), bytes);
    }

    pub fn remove(&self, url: &str) {
        let _ = std::fs::remove_file(self.path(url));
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// Upper bound of one file's period: day files cover 1 day, month files at
/// most 31 days (over-estimating keeps the current month out of the cache,
/// which is what we want while Dukascopy keeps filling it).
fn file_window_secs(source: NativeSource) -> i64 {
    match source {
        NativeSource::Min1 => 86_400,
        NativeSource::Day1 => 31 * 86_400,
    }
}

fn resume_error(e: CoreError, fetched: usize, total: usize) -> CoreError {
    CoreError::InvalidData(format!(
        "{e}; pulled {fetched}/{total} files before failing — wait a minute and \
         rerun the same command (cached files are reused, so the pull resumes)"
    ))
}

/// Decompress LZMA1 (alone) + decode 24-byte big-endian candle records.
pub fn decode_bi5_candles(
    compressed: &[u8],
    period_start: Ts,
    decimals: u32,
) -> CoreResult<Vec<Bar>> {
    if compressed.is_empty() {
        return Ok(Vec::new());
    }
    let mut input = std::io::Cursor::new(compressed);
    let mut raw = Vec::new();
    lzma_rs::lzma_decompress(&mut input, &mut raw).map_err(|e| {
        CoreError::InvalidData(format!(
            "bi5 decompress failed at {}: {e}",
            format_ts(period_start)
        ))
    })?;
    if raw.len() % RECORD_LEN != 0 {
        return Err(CoreError::InvalidData(format!(
            "bi5 payload at {} is {} bytes, not a multiple of {RECORD_LEN}",
            format_ts(period_start),
            raw.len()
        )));
    }
    let factor = bt_core::money::d_powi(D::from(10), u64::from(decimals), "dukascopy scale")?;
    let mut bars = Vec::with_capacity(raw.len() / RECORD_LEN);
    for chunk in raw.chunks_exact(RECORD_LEN) {
        let be32 = |i: usize| -> u32 {
            u32::from_be_bytes([chunk[i], chunk[i + 1], chunk[i + 2], chunk[i + 3]])
        };
        let offset_secs = be32(0) as i64;
        let open = be32(4);
        let close = be32(8);
        let low = be32(12);
        let high = be32(16);
        let volume = f32::from_be_bytes([chunk[20], chunk[21], chunk[22], chunk[23]]);
        let px = |raw_px: u32| -> D { D::from(raw_px) / factor };
        let open_time = period_start + chrono::Duration::seconds(offset_secs);
        bars.push(Bar {
            open_time,
            open: px(open),
            high: px(high),
            low: px(low),
            close: px(close),
            volume: Some(rust_decimal::Decimal::from_f32_retain(volume).unwrap_or(dec!(0))),
        });
    }
    Ok(bars)
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PullReport {
    pub series: BarSeries,
    pub files_fetched: usize,
    pub files_missing: usize,
    /// Files served from the download cache instead of the network.
    pub files_cached: usize,
    pub native_bars: usize,
}

/// Politeness pacing between network fetches: sustained bursts are exactly
/// what the edge throttles; ~4 requests/second stays comfortably under it.
const PACING_MS: u64 = 250;

/// Fetch native candles for the range and resample to the requested
/// timeframe. Missing files (holidays) are skipped and counted; network
/// failures after retries abort with a resume hint — rerunning the same
/// command continues from the cache.
pub fn pull(
    spec: &PullSpec,
    cache: Option<&Bi5Cache>,
    progress: impl Fn(usize, usize),
) -> CoreResult<PullReport> {
    let agent = http_agent();
    warm_up(&agent);
    std::thread::sleep(std::time::Duration::from_millis(300));
    pull_with(
        spec,
        cache,
        PACING_MS,
        |url| fetch_bi5(&agent, url),
        progress,
    )
}

/// Testable core: injectable fetcher, cache and pacing.
pub(crate) fn pull_with(
    spec: &PullSpec,
    cache: Option<&Bi5Cache>,
    pacing_ms: u64,
    mut fetch: impl FnMut(&str) -> CoreResult<Option<Vec<u8>>>,
    mut progress: impl FnMut(usize, usize),
) -> CoreResult<PullReport> {
    let (source, native_interval) = native_source(spec.timeframe_secs);
    let files = enumerate_files(source, spec);
    let total = files.len();
    let window_secs = file_window_secs(source);

    let mut bars: Vec<Bar> = Vec::new();
    let mut fetched = 0usize;
    let mut missing = 0usize;
    let mut cached = 0usize;
    for (i, (url, period_start)) in files.into_iter().enumerate() {
        // Only complete periods are cacheable: the current day/month keeps
        // growing on Dukascopy's side, so a cached copy would silently
        // truncate the range end.
        let complete = period_start + chrono::Duration::seconds(window_secs) <= spec.to;
        let mut from_cache = false;
        let bytes = match cache.filter(|_| complete).and_then(|c| c.get(&url)) {
            Some(bytes) => {
                from_cache = true;
                cached += 1;
                Some(bytes)
            }
            None => {
                let bytes = fetch(&url).map_err(|e| resume_error(e, fetched, total))?;
                if let (Some(c), Some(b)) = (cache, &bytes) {
                    if complete {
                        c.put(&url, b);
                    }
                }
                bytes
            }
        };
        match bytes {
            None => missing += 1,
            Some(bytes) => match decode_bi5_candles(&bytes, period_start, spec.decimals) {
                Ok(new_bars) => {
                    bars.extend(new_bars);
                    fetched += 1;
                }
                Err(e) if from_cache => {
                    // A corrupt cache entry must not poison the pull: evict
                    // it and fetch the real bytes once.
                    if let Some(c) = cache {
                        c.remove(&url);
                    }
                    let bytes = fetch(&url).map_err(|e| resume_error(e, fetched, total))?;
                    match bytes {
                        None => missing += 1,
                        Some(b) => {
                            if let (Some(c), true) = (cache, complete) {
                                c.put(&url, &b);
                            }
                            let new_bars = decode_bi5_candles(&b, period_start, spec.decimals)?;
                            bars.extend(new_bars);
                            fetched += 1;
                        }
                    }
                }
                Err(e) => return Err(e),
            },
        }
        // Pacing applies to network fetches only; cache hits are local disk.
        if !from_cache && pacing_ms > 0 && i + 1 < total {
            std::thread::sleep(std::time::Duration::from_millis(pacing_ms));
        }
        if (i + 1) % 50 == 0 || i + 1 == total {
            progress(i + 1, total);
        }
    }

    bars.retain(|b| b.open_time >= spec.from && b.open_time < spec.to);
    let native_bars = bars.len();
    let series = BarSeries {
        symbol: spec.symbol.clone(),
        interval_secs: native_interval,
        bars,
    };
    let resampled = crate::resample::resample(&series, spec.timeframe_secs)?;
    Ok(PullReport {
        series: resampled,
        files_fetched: fetched,
        files_missing: missing,
        files_cached: cached,
        native_bars,
    })
}

// ---------------------------------------------------------------------------
// spec parsing helpers (CLI-facing)
// ---------------------------------------------------------------------------

/// A lookback like `3` (years), `3y`, `6mo`, `2w`, `30d`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookback {
    Years(u32),
    Months(u32),
    Weeks(u32),
    Days(u32),
}

impl Lookback {
    pub fn to_from_ts(self, now: Ts) -> Ts {
        match self {
            Lookback::Years(n) => now
                .checked_sub_months(chrono::Months::new(12 * n))
                .unwrap_or(now),
            Lookback::Months(n) => now
                .checked_sub_months(chrono::Months::new(n))
                .unwrap_or(now),
            Lookback::Weeks(n) => now - chrono::Duration::weeks(i64::from(n)),
            Lookback::Days(n) => now - chrono::Duration::days(i64::from(n)),
        }
    }
}

/// Parse a lookback token: bare `3` means years; suffixes y/mo/w/d.
pub fn parse_lookback(s: &str) -> CoreResult<Lookback> {
    let t = s.trim().to_lowercase();
    let (num, unit): (&str, &str) = if t.chars().all(|c| c.is_ascii_digit()) {
        (&t[..], "y")
    } else if let Some(prefix) = t.strip_suffix("mo") {
        (prefix, "mo")
    } else {
        (
            t.trim_end_matches(|c: char| c.is_ascii_alphabetic()),
            &t[t.len() - 1..],
        )
    };
    let n: u32 = num.trim().parse().map_err(|_| {
        CoreError::ConfigError(format!(
            "invalid lookback '{s}' (use 3, 3y, 6mo, 2w or 30d)"
        ))
    })?;
    if n == 0 {
        return Err(CoreError::ConfigError("lookback must be positive".into()));
    }
    Ok(match unit {
        "y" => Lookback::Years(n),
        "mo" => Lookback::Months(n),
        "w" => Lookback::Weeks(n),
        "d" => Lookback::Days(n),
        _ => {
            return Err(CoreError::ConfigError(format!(
                "invalid lookback unit in '{s}' (y/mo/w/d)"
            )))
        }
    })
}

/// Parse the `pull-chart` spec argument: `"5,3"` (5-minute, 3 years back),
/// `"1h,2y"`, `"1d"`. Bare numbers are minutes.
pub fn parse_tf_spec(spec: &str) -> CoreResult<(i64, Option<Lookback>)> {
    let (tf_part, lb_part) = match spec.split_once(',') {
        Some((a, b)) => (a.trim(), Some(b.trim())),
        None => (spec.trim(), None),
    };
    let tf = if tf_part.chars().all(|c| c.is_ascii_digit()) && !tf_part.is_empty() {
        bt_core::time::parse_interval(&format!("{tf_part}m"))?
    } else {
        bt_core::time::parse_interval(tf_part)?
    };
    let lb = lb_part.map(parse_lookback).transpose()?;
    Ok((tf, lb))
}

/// Canonical timeframe file-name fragment ("5m", "1h", "1d").
pub fn tf_name(secs: i64) -> String {
    if secs % 86_400 == 0 {
        format!("{}d", secs / 86_400)
    } else if secs % 3_600 == 0 {
        format!("{}h", secs / 3_600)
    } else {
        format!("{}m", secs / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(y: i32, m: u32, d: u32, h: u32) -> Ts {
        Ts::from_naive_utc_and_offset(
            chrono::NaiveDate::from_ymd_opt(y, m, d)
                .unwrap()
                .and_hms_opt(h, 0, 0)
                .unwrap(),
            chrono::Utc,
        )
    }

    #[test]
    fn url_builder_uses_zero_indexed_month() {
        let spec = PullSpec {
            symbol: "EURUSD".into(),
            timeframe_secs: 300,
            from: ts(2024, 1, 2, 0),
            to: ts(2024, 1, 3, 0),
            side: Side::Bid,
            decimals: 5,
        };
        let files = enumerate_files(NativeSource::Min1, &spec);
        // Jan 2 = month index 0 (NOT padded — Dukascopy's raw scheme)
        assert!(
            files[0].0.contains("/2024/00/02/BID_candles_min_1.bi5"),
            "{}",
            files[0].0
        );
        assert_eq!(files.len(), 2, "two day files for a 1-day range");
        // February: month index 1
        let spec_feb = PullSpec {
            from: ts(2024, 2, 1, 0),
            to: ts(2024, 2, 2, 0),
            ..spec
        };
        let files = enumerate_files(NativeSource::Min1, &spec_feb);
        assert!(files[0].0.contains("/2024/01/01/"), "{}", files[0].0);
    }

    #[test]
    fn day_files_are_monthly() {
        let spec = PullSpec {
            symbol: "X".into(),
            timeframe_secs: 86_400,
            from: ts(2024, 3, 1, 0),
            to: ts(2024, 4, 1, 0),
            side: Side::Ask,
            decimals: 5,
        };
        let files = enumerate_files(NativeSource::Day1, &spec);
        assert_eq!(files.len(), 2, "March + April month files");
        assert!(
            files[0].0.contains("/X/2024/02/ASK_candles_day_1.bi5"),
            "{}",
            files[0].0
        );
    }

    #[test]
    fn decode_bi5_roundtrip() {
        // one 1-min candle: 60s offset, open 1.10000 (110000), close 1.10100,
        // low 1.09900, high 1.10150, volume 1.25
        let mut payload = Vec::new();
        let be32 = |v: u32| v.to_be_bytes();
        payload.extend_from_slice(&be32(60));
        payload.extend_from_slice(&be32(110_000));
        payload.extend_from_slice(&be32(110_100));
        payload.extend_from_slice(&be32(109_900));
        payload.extend_from_slice(&be32(110_150));
        payload.extend_from_slice(&1.25f32.to_be_bytes());
        let mut compressed = Vec::new();
        lzma_rs::lzma_compress(&mut std::io::Cursor::new(&payload), &mut compressed).unwrap();
        let bars = decode_bi5_candles(&compressed, ts(2024, 1, 2, 0), 5).unwrap();
        assert_eq!(bars.len(), 1);
        let b = &bars[0];
        assert_eq!(
            b.open_time,
            ts(2024, 1, 2, 0) + chrono::Duration::seconds(60)
        );
        assert_eq!(b.open.normalize().to_string(), "1.1");
        assert_eq!(b.close.normalize().to_string(), "1.101");
        assert_eq!(b.low.normalize().to_string(), "1.099");
        assert_eq!(b.high.normalize().to_string(), "1.1015");
        assert_eq!(b.volume, Some(D::from_f64_retain(1.25).unwrap()));
    }

    #[test]
    fn empty_and_corrupt_bi5() {
        assert!(decode_bi5_candles(&[], ts(2024, 1, 2, 0), 5)
            .unwrap()
            .is_empty());
        assert!(decode_bi5_candles(&[1, 2, 3], ts(2024, 1, 2, 0), 5).is_err());
    }

    #[test]
    fn lookback_and_tf_parsing() {
        assert_eq!(
            parse_tf_spec("5,3").unwrap(),
            (300, Some(Lookback::Years(3)))
        );
        assert_eq!(
            parse_tf_spec("5m,6mo").unwrap(),
            (300, Some(Lookback::Months(6)))
        );
        assert_eq!(
            parse_tf_spec("1h,2w").unwrap(),
            (3600, Some(Lookback::Weeks(2)))
        );
        assert_eq!(
            parse_tf_spec("1d,90d").unwrap(),
            (86_400, Some(Lookback::Days(90)))
        );
        assert_eq!(parse_tf_spec("15").unwrap(), (900, None));
        assert_eq!(parse_tf_spec("1h").unwrap(), (3600, None));
        assert!(parse_tf_spec("0,3").is_err());
        assert!(parse_tf_spec("5,x").is_err());
    }

    #[test]
    fn lookback_to_from() {
        let now = ts(2024, 3, 15, 12);
        assert_eq!(Lookback::Days(10).to_from_ts(now), ts(2024, 3, 5, 12));
        let back3y = Lookback::Years(3).to_from_ts(now);
        assert_eq!(back3y.year(), 2021);
        assert_eq!(back3y.month(), 3);
    }

    #[test]
    fn tf_names() {
        assert_eq!(tf_name(300), "5m");
        assert_eq!(tf_name(3600), "1h");
        assert_eq!(tf_name(14_400), "4h");
        assert_eq!(tf_name(86_400), "1d");
        assert_eq!(tf_name(604_800), "7d");
    }

    #[test]
    fn native_source_mapping() {
        assert_eq!(native_source(300), (NativeSource::Min1, 60));
        assert_eq!(native_source(3600), (NativeSource::Min1, 60));
        assert_eq!(native_source(14_400), (NativeSource::Min1, 60));
        assert_eq!(native_source(86_400), (NativeSource::Day1, 86_400));
    }

    #[test]
    fn enumerate_lists_every_calendar_day() {
        // Weekends DO serve files on Dukascopy (verified against the live
        // feed: FX candles are published Saturdays/Sundays), so every
        // calendar day must be requested — 404s are counted as missing.
        let spec = PullSpec {
            symbol: "EURUSD".into(),
            timeframe_secs: 300,
            from: ts(2025, 9, 8, 0), // Monday
            to: ts(2025, 9, 15, 0),
            side: Side::Bid,
            decimals: 5,
        };
        let files = enumerate_files(NativeSource::Min1, &spec);
        assert_eq!(files.len(), 8, "Sep 8..15 inclusive");
        assert!(
            files.iter().any(|(u, _)| u.contains("/2025/08/13/")),
            "Saturday Sep 13 (month 08, zero-indexed) must be requested: {files:?}"
        );
    }

    /// One valid 1-minute bi5 record, compressed (same payload as the
    /// decode round-trip test).
    fn ok_bytes() -> Vec<u8> {
        let mut payload = Vec::new();
        let be32 = |v: u32| v.to_be_bytes();
        payload.extend_from_slice(&be32(60));
        payload.extend_from_slice(&be32(110_000));
        payload.extend_from_slice(&be32(110_100));
        payload.extend_from_slice(&be32(109_900));
        payload.extend_from_slice(&be32(110_150));
        payload.extend_from_slice(&1.25f32.to_be_bytes());
        let mut compressed = Vec::new();
        lzma_rs::lzma_compress(&mut std::io::Cursor::new(&payload), &mut compressed).unwrap();
        compressed
    }

    fn day_spec() -> PullSpec {
        PullSpec {
            symbol: "EURUSD".into(),
            timeframe_secs: 60,
            from: ts(2024, 1, 1, 0),
            to: ts(2024, 1, 3, 0),
            side: Side::Bid,
            decimals: 5,
        }
    }

    fn temp_cache(tag: &str) -> Bi5Cache {
        let dir = std::env::temp_dir().join(format!("bt_bi5_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Bi5Cache::new(&dir).unwrap()
    }

    #[test]
    fn pull_resumes_from_cache() {
        let cache = temp_cache("resume");
        let spec = day_spec();
        // Jan 1 + Jan 2 are complete periods; Jan 3 404s (and absence is
        // never cached, since the live feed keeps filling that day).
        let counter = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let mk = || {
            let counter = std::rc::Rc::clone(&counter);
            move |url: &str| -> CoreResult<Option<Vec<u8>>> {
                counter.set(counter.get() + 1);
                if url.contains("/2024/00/03/") {
                    Ok(None)
                } else {
                    Ok(Some(ok_bytes()))
                }
            }
        };
        let report = pull_with(&spec, Some(&cache), 0, mk(), |_, _| {}).unwrap();
        assert_eq!(report.files_fetched, 2);
        assert_eq!(report.files_missing, 1);
        assert_eq!(report.files_cached, 0);
        assert_eq!(counter.get(), 3, "every file hit the network once");

        let report2 = pull_with(&spec, Some(&cache), 0, mk(), |_, _| {}).unwrap();
        assert_eq!(report2.files_cached, 2, "complete periods come from cache");
        assert_eq!(report2.files_missing, 1);
        assert_eq!(counter.get(), 4, "only the 404 is re-attempted");
        let _ = std::fs::remove_dir_all(cache.dir());
    }

    #[test]
    fn incomplete_period_is_never_cached() {
        let cache = temp_cache("incomplete");
        // `to` lands mid-day: Jan 2's file is still growing on the feed.
        let spec = PullSpec {
            from: ts(2024, 1, 1, 0),
            to: ts(2024, 1, 2, 12),
            ..day_spec()
        };
        let files = enumerate_files(NativeSource::Min1, &spec);
        let fetch = |url: &str| -> CoreResult<Option<Vec<u8>>> {
            if url.contains("/2024/00/02/") {
                Ok(Some(ok_bytes()))
            } else {
                Ok(None)
            }
        };
        let report = pull_with(&spec, Some(&cache), 0, fetch, |_, _| {}).unwrap();
        assert_eq!(report.files_fetched, 1);
        assert!(
            cache.get(&files[1].0).is_none(),
            "the growing day must not be cached"
        );
        let _ = std::fs::remove_dir_all(cache.dir());
    }

    #[test]
    fn corrupt_cache_entry_is_evicted_and_refetched() {
        let cache = temp_cache("corrupt");
        let spec = day_spec();
        let files = enumerate_files(NativeSource::Min1, &spec);
        cache.put(&files[0].0, &[1, 2, 3]); // garbage: fails LZMA decode

        let fetch = |_: &str| -> CoreResult<Option<Vec<u8>>> { Ok(Some(ok_bytes())) };
        let report = pull_with(&spec, Some(&cache), 0, fetch, |_, _| {}).unwrap();
        assert_eq!(report.files_fetched, 3, "bad entry evicted, refetched");
        assert!(cache.get(&files[0].0).is_some(), "valid bytes re-cached");

        let report2 = pull_with(&spec, Some(&cache), 0, fetch, |_, _| {}).unwrap();
        assert_eq!(
            report2.files_cached, 2,
            "the incomplete Jan 3 file is never cached"
        );
        let _ = std::fs::remove_dir_all(cache.dir());
    }

    #[test]
    fn fetch_failure_carries_resume_hint() {
        let spec = day_spec();
        let mut n = 0;
        let fetch = move |_: &str| -> CoreResult<Option<Vec<u8>>> {
            n += 1;
            if n == 2 {
                Err(CoreError::InvalidData(
                    "HTTP 503 (Dukascopy edge throttling; retries exhausted)".into(),
                ))
            } else {
                Ok(Some(ok_bytes()))
            }
        };
        let err = pull_with(&spec, None, 0, fetch, |_, _| {}).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("rerun the same command"), "{msg}");
        assert!(msg.contains("1/3"), "fetched count in: {msg}");
    }
}
