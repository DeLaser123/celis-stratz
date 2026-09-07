//! `backtest ai` subcommands: compile / review / explain / ask / status /
//! ledger / login. Everything AI-related in the CLI funnels through here and
//! through the bt-ai gateway (budgets, cache, ledger).

use bt_ai::compile::{compile_strategy, CompileInput};
use bt_ai::gateway::{Gateway, GatewayConfig};
use bt_ai::keys::KeySource;
use bt_ai::prompts::{
    ask_system_prompt, bound_text, explain_system_prompt, review_system_prompt, DataContext,
    SPEC_GRAMMAR,
};
use bt_core::error::{CoreError, CoreResult};
use bt_harness::project::{Project, StrategyVersion};
use bt_harness::settings::HarnessSettings;

pub struct AiFlags<'a> {
    pub api_key: Option<&'a str>,
    pub model: Option<&'a str>,
    pub base_url: Option<&'a str>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f64>,
    pub no_cache: bool,
}

/// Resolve settings + key + gateway.
pub struct AiContext {
    pub gateway: Gateway,
    pub key_source: KeySource,
    pub settings: HarnessSettings,
    pub cache_dir: Option<std::path::PathBuf>,
    pub ledger_path: Option<std::path::PathBuf>,
}

/// `require_key=false` lets `ai status`/`ai ledger` run before any key is configured.
pub fn setup_with(
    project: Option<Project>,
    flags: &AiFlags<'_>,
    require_key: bool,
) -> CoreResult<AiContext> {
    let settings = match &project {
        Some(p) => {
            let path = p.config_path();
            if path.exists() {
                HarnessSettings::load(&path)?
            } else {
                HarnessSettings::default()
            }
        }
        None => HarnessSettings::default(),
    };

    let (api_key, key_source) = bt_ai::resolve_api_key(flags.api_key);

    let mut cfg: GatewayConfig = settings.to_gateway_config();
    if let Some(m) = flags.model {
        cfg.model = m.to_string();
    }
    if let Some(u) = flags.base_url {
        cfg.base_url = Some(u.to_string());
    }
    if let Some(t) = flags.max_tokens {
        cfg.max_tokens = t;
    }
    if let Some(t) = flags.temperature {
        cfg.temperature = t;
    }

    let (cache, cache_dir) = match (&project, flags.no_cache) {
        (Some(p), false) => {
            let dir = p.state_dir().join("cache").join("ai");
            (Some(bt_ai::ResponseCache::new(&dir)?), Some(dir))
        }
        _ => (None, None),
    };
    let (ledger, ledger_path) = match &project {
        Some(p) => {
            let path = p.state_dir().join("ai").join("ledger.jsonl");
            (Some(bt_ai::LedgerWriter::new(&path)?), Some(path))
        }
        None => (None, None),
    };

    let key = match api_key {
        Some(k) => k,
        None if !require_key => {
            // Placeholder: status/ledger never call the provider, and any
            // accidental real call fails loudly at the HTTP layer.
            String::from("<no key configured>")
        }
        None => {
            return Err(CoreError::InvalidData(format!(
                "no AI API key found. Provide one with --api-key, the {} environment variable, \
                 or `backtest ai login --api-key <key>`",
                bt_ai::keys::ENV_VAR
            )))
        }
    };

    let gateway = Gateway::new(cfg, &key, cache, ledger);
    Ok(AiContext {
        gateway,
        key_source,
        settings,
        cache_dir,
        ledger_path,
    })
}

/// Build the DataContext from the project's data folder (cheap sniffing).
pub fn data_context(project: &Project, settings: &HarnessSettings) -> CoreResult<DataContext> {
    let files = project.data_files()?;
    let mut symbols: Vec<String> = Vec::new();
    let mut rows = 0usize;
    let mut tf = "1h".to_string();
    for f in &files {
        for s in &f.symbols {
            if !symbols.contains(s) {
                symbols.push(s.clone());
            }
        }
        rows += f.rows;
        if let Some(hint) = &f.timeframe_hint {
            tf = hint.clone();
        }
    }
    // Period: sniffed cheaply from the first/last line of the first file.
    let (start, end) = files
        .first()
        .and_then(|f| sniff_period(&f.path))
        .unwrap_or_else(|| ("unknown".into(), "unknown".into()));
    let _ = settings;
    Ok(DataContext {
        symbols,
        base_timeframe: tf,
        bars: rows,
        start,
        end,
        timezone: "UTC".into(),
    })
}

fn sniff_period(path: &std::path::Path) -> Option<(String, String)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let header = lines.next()?;
    let ts_idx = header.split(',').position(|c| c.trim() == "timestamp")?;
    let first = lines.next()?.split(',').nth(ts_idx)?.trim().to_string();
    let mut last = first.clone();
    for line in lines {
        if let Some(ts) = line.split(',').nth(ts_idx) {
            last = ts.trim().to_string();
        }
    }
    Some((first, last))
}

/// Gather bounded project notes for AI context.
pub fn project_notes(project: &Project) -> String {
    let mut out = String::new();
    for p in project.note_files() {
        if let Ok(text) = std::fs::read_to_string(&p) {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("note");
            out.push_str(&format!(
                "--- {name} ---\n{}\n\n",
                bound_text(&text, 4_000, name)
            ));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

pub fn cmd_compile(
    project: &Project,
    ai: &mut AiContext,
    strategy_version: Option<&str>,
    dry_run: bool,
) -> CoreResult<()> {
    let version: StrategyVersion = project.resolve_strategy(strategy_version)?;
    let md_path = version.path.join("strategy.md");
    let md = std::fs::read_to_string(&md_path).map_err(|e| {
        CoreError::InvalidData(format!(
            "read {}: {e} (the AI compiles strategy.md — create it first)",
            md_path.display()
        ))
    })?;
    let notes = project_notes(project);
    let ctx = data_context(project, &ai.settings)?;

    // Base timeframe seconds for the compiler validation.
    let base_secs = bt_core::time::parse_interval(&ctx.base_timeframe).unwrap_or(3600);

    let input = CompileInput {
        strategy_md: &md,
        notes: &notes,
        context: &ctx,
        base_secs,
        max_repair_rounds: ai.settings.ai.max_repair_rounds,
        strategy_version: &version.name,
    };

    let result = compile_strategy(&mut ai.gateway, &input, dry_run)?;
    match result {
        Err(report) => {
            println!("DRY RUN — no API call made");
            println!("  model:          {}", report.model);
            println!(
                "  prompt chars:   {} (system {})",
                report.prompt_chars, report.system_chars
            );
            println!("  prompt hash:    {}", report.prompt_hash);
            println!("  repair rounds:  up to {}", report.max_repair_rounds);
            println!("  prompt preview:\n{}", report.preview);
            Ok(())
        }
        Ok(outcome) => {
            let yaml_path = version.path.join("strategy.yaml");
            std::fs::write(&yaml_path, &outcome.spec_yaml)?;
            let assumptions_path = version.path.join("assumptions.md");
            let assumptions_md = format!(
                "# Assumptions — compiled by AI (strategy version {})\n\n\
                 Compiler rounds: {} · cache hits: {} · ledger entries: {:?}\n\
                 These record every decision the compiler had to make because the\n\
                 strategy description was ambiguous. The engine validated the spec.\n\n{}\n",
                version.name,
                outcome.rounds,
                outcome.cache_hits,
                outcome.ledger_ids,
                outcome
                    .assumptions
                    .iter()
                    .map(|a| format!("- {a}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            std::fs::write(&assumptions_path, assumptions_md)?;
            println!(
                "VALID: strategy '{}' compiled in {} round(s)",
                outcome.strategy_name, outcome.rounds
            );
            println!("  spec:        {}", yaml_path.display());
            println!("  assumptions: {}", assumptions_path.display());
            println!("  ledger ids:  {:?}", outcome.ledger_ids);
            if outcome.cache_hits > 0 {
                println!("  cache hits:  {}", outcome.cache_hits);
            }
            println!(
                "  tokens:      {} used this command",
                ai.gateway.tokens_used()
            );
            Ok(())
        }
    }
}

pub fn cmd_review(
    project: &Project,
    ai: &mut AiContext,
    strategy_version: Option<&str>,
    dry_run: bool,
) -> CoreResult<()> {
    let version = project.resolve_strategy(strategy_version)?;
    let spec_path = version.path.join("strategy.yaml");
    let spec_text = std::fs::read_to_string(&spec_path)
        .map_err(|e| CoreError::InvalidData(format!("read {}: {e}", spec_path.display())))?;
    let md_text = std::fs::read_to_string(version.path.join("strategy.md"))
        .unwrap_or_else(|_| "(no strategy.md)".into());
    let notes = project_notes(project);
    let ctx = data_context(project, &ai.settings)?;

    let user = format!(
        "{}\nCOMPILED SPEC:\n{}\n\nORIGINAL TRADER TEXT:\n{}\n{}",
        ctx.render(),
        bound_text(&spec_text, 12_000, "spec"),
        bound_text(&md_text, 8_000, "strategy.md"),
        if notes.is_empty() {
            String::new()
        } else {
            format!("\nPROJECT NOTES:\n{notes}")
        }
    );
    let outcome = ai
        .gateway
        .run("review", &review_system_prompt(), &user, false, dry_run)?;
    if dry_run {
        println!("DRY RUN — no API call made");
        println!("  prompt hash: {}", outcome.prompt_hash);
        println!("  prompt chars: {}", outcome.prompt_chars);
    } else {
        println!("{}", outcome.text);
        println!(
            "\n[ledger #{} · {} tokens · cache_hit={}]",
            outcome.ledger_id,
            outcome.usage.total(),
            outcome.cache_hit
        );
    }
    Ok(())
}

pub fn cmd_explain(
    _project: Option<&Project>,
    ai: &mut AiContext,
    results: &std::path::Path,
    dry_run: bool,
) -> CoreResult<()> {
    let metrics_text = std::fs::read_to_string(results.join("metrics.json"))
        .map_err(|e| CoreError::InvalidData(format!("metrics.json: {e}")))?;
    let summary_text = std::fs::read_to_string(results.join("summary.json"))
        .map_err(|e| CoreError::InvalidData(format!("summary.json: {e}")))?;
    let trades_text = std::fs::read_to_string(results.join("trades.csv"))
        .map(|t| bound_text(&t, 6_000, "trades.csv"))
        .unwrap_or_else(|_| "(no trades)".into());

    let context = format!(
        "ARTIFACTS (verbatim; quote numbers only from here):\n\nsummary.json:\n{summary_text}\n\nmetrics.json:\n{}\n\ntrades.csv (bounded):\n{trades_text}\n",
        bound_text(&metrics_text, 16_000, "metrics.json")
    );
    let user = format!("Explain this backtest result to the researcher who ran it.\n\n{context}");
    let outcome = ai
        .gateway
        .run("explain", &explain_system_prompt(), &user, false, dry_run)?;
    if dry_run {
        println!("DRY RUN — no API call made");
        println!("  prompt hash: {}", outcome.prompt_hash);
        println!("  prompt chars: {}", outcome.prompt_chars);
    } else {
        let response = &outcome.text;
        println!("{response}");
        let unverified = bt_ai::prompts::unverified_numbers(response, &context);
        if !unverified.is_empty() {
            println!(
                "\n[stratz verification] numbers NOT found in the provided artifacts (verify manually): {}",
                unverified.join(", ")
            );
        } else {
            println!("\n[stratz verification] all quoted numbers found in the artifacts");
        }
        println!(
            "[ledger #{} · {} tokens · cache_hit={}]",
            outcome.ledger_id,
            outcome.usage.total(),
            outcome.cache_hit
        );
    }
    Ok(())
}

pub fn cmd_ask(
    project: &Project,
    ai: &mut AiContext,
    question: &str,
    dry_run: bool,
) -> CoreResult<()> {
    let notes = project_notes(project);
    let ctx = data_context(project, &ai.settings)?;
    let user = format!(
        "{}\nPROJECT MATERIAL:\n{}\nQUESTION:\n{question}",
        ctx.render(),
        if notes.is_empty() {
            "(none)".into()
        } else {
            notes
        }
    );
    let outcome = ai
        .gateway
        .run("ask", &ask_system_prompt(), &user, false, dry_run)?;
    if dry_run {
        println!("DRY RUN — no API call made");
        println!("  prompt hash: {}", outcome.prompt_hash);
        println!("  prompt chars: {}", outcome.prompt_chars);
    } else {
        println!("{}", outcome.text);
        println!(
            "\n[ledger #{} · {} tokens · cache_hit={}]",
            outcome.ledger_id,
            outcome.usage.total(),
            outcome.cache_hit
        );
    }
    Ok(())
}

pub fn cmd_status(ai: &AiContext, project: Option<&Project>) -> CoreResult<()> {
    println!("provider:    {}", ai.settings.ai.provider);
    println!("model:       {}", ai.settings.ai.model);
    println!("base url:    {}", ai.gateway.config().effective_base_url());
    println!("key source:  {}", ai.key_source.label());
    if ai.key_source == KeySource::None {
        println!(
            "             (provide via --api-key, {}, or `backtest ai login --api-key <key>`)",
            bt_ai::keys::ENV_VAR
        );
    }
    println!("max tokens:  {}", ai.settings.ai.max_tokens);
    println!("temperature: {}", ai.settings.ai.temperature);
    println!(
        "budget:      {} tokens per command",
        ai.settings.ai.budget_tokens_per_command
    );
    println!("repair cap:  {} rounds", ai.settings.ai.max_repair_rounds);
    if let Some(d) = &ai.cache_dir {
        println!(
            "cache:       {} ({} entries)",
            d.display(),
            bt_ai::ResponseCache::new(d).map(|c| c.len()).unwrap_or(0)
        );
    } else {
        println!("cache:       disabled (no project / --no-cache)");
    }
    if let Some(p) = &ai.ledger_path {
        println!(
            "ledger:      {} ({} entries)",
            p.display(),
            bt_ai::LedgerWriter::new(p).unwrap().entry_count()
        );
    }
    if let Some(p) = project {
        let spec = p.resolve_strategy(None).ok();
        if let Some(v) = spec {
            println!(
                "strategy:    {} (spec: {})",
                v.name,
                if v.has_spec { "yes" } else { "no" }
            );
        }
        println!(
            "grammar:     {} bytes of spec grammar in prompts",
            SPEC_GRAMMAR.len()
        );
    }
    Ok(())
}

pub fn cmd_ledger(ai: &AiContext, limit: usize) -> CoreResult<()> {
    let Some(path) = &ai.ledger_path else {
        println!("no ledger (not inside a project)");
        return Ok(());
    };
    let w = bt_ai::LedgerWriter::new(path)?;
    let entries = w.read_last(limit)?;
    if entries.is_empty() {
        println!("ledger is empty: {}", path.display());
        return Ok(());
    }
    println!(
        "ledger: {} (showing last {} of {})",
        path.display(),
        entries.len(),
        w.entry_count()
    );
    for (id, line) in entries {
        // Compact one-line rendering of selected fields.
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
            println!(
                "#{id} {} {} [{}] {} temp={} tokens={}/{} cache={} {}ms status={} purpose={}",
                v.get("ts").and_then(|x| x.as_str()).unwrap_or(""),
                v.get("model").and_then(|x| x.as_str()).unwrap_or(""),
                v.get("provider").and_then(|x| x.as_str()).unwrap_or(""),
                &v.get("prompt_hash").and_then(|x| x.as_str()).unwrap_or("")[..12],
                v.get("temperature").and_then(|x| x.as_f64()).unwrap_or(0.0),
                v.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
                v.get("completion_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0),
                v.get("cache_hit")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false),
                v.get("latency_ms").and_then(|x| x.as_u64()).unwrap_or(0),
                v.get("status").and_then(|x| x.as_str()).unwrap_or(""),
                v.get("purpose").and_then(|x| x.as_str()).unwrap_or(""),
            );
        } else {
            println!("#{id} {line}");
        }
    }
    Ok(())
}

pub fn cmd_login(api_key: Option<&str>, clear: bool) -> CoreResult<()> {
    if clear {
        bt_ai::clear_key()?;
        println!("keyring entry cleared");
        return Ok(());
    }
    let Some(key) = api_key else {
        return Err(CoreError::InvalidData(
            "give --api-key <key> to save, or --clear to remove".into(),
        ));
    };
    bt_ai::store_key(key)?;
    println!(
        "API key saved to the OS keyring (service '{}')",
        bt_ai::keys::KEYRING_SERVICE
    );
    println!("it is NOT stored in any project folder");
    Ok(())
}
