//! The deterministic event-loop engine (reference implementation, spec §37).
//!
//! Per (timestamp, symbol) processing order is fixed and documented:
//!   1. market event
//!   2. pending market orders fill at the open (next_open model)
//!   3. intrabar protective triggers (SL/TP/trailing) with ambiguity policy
//!   4. financing at close
//!   5. MAE/MFE excursion update
//!   6. decision phase: indicators update, strategy evaluates, risk validates,
//!      orders are created (filled now under current_close, else queued)
//!   7. trailing stop update at close
//!   8. margin check, then account/equity snapshot
//!
//! The strategy only *requests*; execution and accounting *decide*.

use crate::config::{EndPolicy, EngineConfig, ExecutionModel, IndicatorModeConfig};
use bt_accounting::account::{Account, ApplyOutcome};
use bt_accounting::trade::TradeBuilder;
use bt_analytics::mc::run_monte_carlo;
use bt_analytics::metrics::{compute_metrics, EquityPoint, MetricsInput};
use bt_core::error::{CoreError, CoreResult};
use bt_core::event::{Event, EventRecord, EventSink, Sequencer};
use bt_core::instrument::Instrument;
use bt_core::ledger::{FillRecord, TradeRecord};
use bt_core::order::{
    IdGenerator, Order, OrderReason, OrderStatus, OrderType, PositionEffect, Side,
};
use bt_core::time::{floor_to_interval, Ts};
use bt_core::D;
use bt_data::{Bar, BarSeries, Dataset};
use bt_execution::fills::build_fill;
use bt_execution::intrabar::{evaluate_exit, AmbiguityPolicy, BarSpan, ExitDecision, ExitTriggers};
use bt_execution::CostModels;
use bt_risk::{RiskContext, RiskDecision, RiskEngine, SizingMode};
use bt_strategy::expr::{AccountView, EvalContext, PositionView, Value};
use bt_strategy::runtime::{
    evaluate as eval_intent, BarAccess, CompiledStrategy, IndicatorMode, IndicatorRuntime, Intent,
};
use bt_strategy::spec::{EntryOrderDef, PriceSpec, StopLossDef, TakeProfitDef, TrailingDef};
use bt_strategy::StrategySpec;
use rust_decimal_macros::dec;
use std::collections::{BTreeMap, VecDeque};

/// Entry trigger prices resolved at decision time.
#[derive(Debug, Clone, Copy, Default)]
pub struct EntryPrices {
    pub limit_price: Option<D>,
    pub stop_price: Option<D>,
}

pub struct RunResult {
    pub trades: Vec<TradeRecord>,
    pub orders: Vec<Order>,
    pub fills: Vec<FillRecord>,
    pub equity_curve: Vec<EquityPoint>,
    pub ledger: Vec<bt_core::ledger::LedgerEntry>,
    pub metrics: bt_analytics::MetricsReport,
    pub monte_carlo: bt_analytics::mc::McReport,
    pub experiment: serde_json::Value,
    pub summary: serde_json::Value,
    pub experiment_id: String,
    pub result_hash: String,
}

struct SymbolState<'a> {
    instrument: Instrument,
    base: &'a BarSeries,
    htfs: BTreeMap<String, BarSeries>,
    htf_cursor: BTreeMap<String, usize>,
    indicators: IndicatorRuntime,
    pending: VecDeque<u64>,
    protective: Vec<u64>,
    trade: Option<TradeBuilder>,
}

pub struct SimulationEngine;

impl SimulationEngine {
    pub fn run(
        dataset: &Dataset,
        spec: &StrategySpec,
        cfg: &EngineConfig,
        signals: &BTreeMap<String, Vec<(Ts, D)>>,
        event_sink: &mut dyn EventSink,
    ) -> CoreResult<RunResult> {
        let base_secs = dataset.base_interval_secs;
        let compiled = bt_strategy::runtime::compile(spec, base_secs).map_err(|errs| {
            CoreError::StrategyError(format!(
                "strategy INVALID ({} error(s)):\n- {}",
                errs.len(),
                errs.join("\n- ")
            ))
        })?;

        for s in &compiled.symbols {
            if !dataset.series.contains_key(s) {
                return Err(CoreError::UnknownSymbol(format!(
                    "strategy references symbol '{s}' which is not present in the data"
                )));
            }
        }
        let symbols = compiled.symbols.clone();
        let instruments = cfg.resolve_instruments(&symbols);
        let engine_version = env!("CARGO_PKG_VERSION").to_string();

        let indicator_mode = match cfg.runtime.indicator_mode {
            IndicatorModeConfig::Precompute => IndicatorMode::Precompute,
            IndicatorModeConfig::Streaming => IndicatorMode::Streaming,
        };
        let policy: AmbiguityPolicy =
            match &cfg.execution.intrabar_policy {
                AmbiguityPolicy::Explicit(_) => match &cfg.execution.explicit_priority {
                    Some(list) if !list.is_empty() => AmbiguityPolicy::Explicit(list.clone()),
                    _ => return Err(CoreError::ConfigError(
                        "execution.intrabar_policy = explicit requires execution.explicit_priority"
                            .into(),
                    )),
                },
                other => other.clone(),
            };

        // Per-symbol state (HTF resampling + indicator runtime).
        let mut states: BTreeMap<String, SymbolState> = BTreeMap::new();
        for symbol in &symbols {
            let base = &dataset.series[symbol];
            let mut htfs = BTreeMap::new();
            for (tf_name, tf_secs) in &compiled.htfs {
                htfs.insert(
                    tf_name.clone(),
                    bt_data::resample::resample(base, *tf_secs)?,
                );
            }
            let indicators = IndicatorRuntime::build(&compiled, indicator_mode, base, &htfs)?;
            states.insert(
                symbol.clone(),
                SymbolState {
                    instrument: instruments[symbol].clone(),
                    base,
                    htfs,
                    htf_cursor: BTreeMap::new(),
                    indicators,
                    pending: VecDeque::new(),
                    protective: Vec::new(),
                    trade: None,
                },
            );
        }

        let mut eng = Engine {
            compiled,
            cfg,
            base_secs,
            signals,
            actions: &dataset.actions,
            policy,
            instruments,
            costs: cfg.costs.clone(),
            account: Account::new(cfg.account.starting_capital, cfg.account.leverage),
            book: BTreeMap::new(),
            fills: Vec::new(),
            trades: Vec::new(),
            equity_curve: Vec::new(),
            planned: BTreeMap::new(),
            states,
            ids: IdGenerator::new(),
            trade_ids: IdGenerator::new(),
            entry_fills_today: 0,
            peak_equity: cfg.account.starting_capital,
            day_start_equity: cfg.account.starting_capital,
            current_day: None,
            sink: event_sink,
            seq: Sequencer::new(),
            engine_version,
            fill_volume: None,
        };

        // RunStarted + data warnings
        let first_ts = dataset
            .series
            .values()
            .next()
            .and_then(|s| s.bars.first())
            .map(|b| b.open_time)
            .unwrap_or(Ts::UNIX_EPOCH);
        eng.emit(
            first_ts,
            Event::RunStarted {
                engine_version: eng.engine_version.clone(),
                strategy: eng.compiled.name.clone(),
                symbols: symbols.clone(),
                timeframe: format!("{}s", base_secs),
                starting_capital: cfg.account.starting_capital,
                execution_model: format!("{:?}", cfg.execution.model).to_lowercase(),
                intrabar_policy: format!("{:?}", eng.policy).to_lowercase(),
            },
        );
        for issue in &dataset.report.issues {
            eng.emit(
                first_ts,
                Event::DataWarning {
                    code: issue.code.as_str().to_string(),
                    detail: issue.detail.clone(),
                },
            );
        }

        // ---- timeline: union of (open_time, symbol), sorted ------------------
        let mut timeline: BTreeMap<Ts, Vec<String>> = BTreeMap::new();
        for (symbol, series) in &dataset.series {
            for bar in &series.bars {
                timeline
                    .entry(bar.open_time)
                    .or_default()
                    .push(symbol.clone());
            }
        }
        for v in timeline.values_mut() {
            v.sort();
        }

        for (ts, symbols_at_ts) in &timeline {
            for symbol in symbols_at_ts.clone() {
                let bar_idx = match eng.states[symbol.as_str()]
                    .base
                    .bars
                    .iter()
                    .position(|b| b.open_time == *ts)
                {
                    Some(i) => i,
                    None => continue,
                };
                eng.process_bar(*ts, &symbol, bar_idx)?;
            }
            eng.snapshot(*ts)?;
        }

        // ---- end of data ------------------------------------------------------
        eng.finish_data()?;
        let final_equity = eng
            .equity_curve
            .last()
            .map(|p| p.equity)
            .unwrap_or(cfg.account.starting_capital);
        eng.emit(
            eng.equity_curve
                .last()
                .map(|p| p.ts)
                .unwrap_or(Ts::UNIX_EPOCH),
            Event::RunFinished {
                final_equity,
                total_events: eng.seq.total(),
            },
        );

        let mc_cfg = cfg.analytics.monte_carlo;
        let mc = run_monte_carlo(
            &eng.trades,
            cfg.account.starting_capital,
            mc_cfg.paths,
            mc_cfg.seed,
            mc_cfg.method,
            mc_cfg.ruin_threshold_pct,
        );
        let metrics = compute_metrics(&MetricsInput {
            equity_curve: &eng.equity_curve,
            trades: &eng.trades,
            initial_capital: cfg.account.starting_capital,
            bar_secs: base_secs,
            config: &cfg.analytics,
            risk_of_ruin_pct: mc.risk_of_ruin_pct,
        });

        let experiment = crate::experiment::build_experiment(
            &eng.engine_version,
            dataset,
            spec,
            cfg,
            &eng.instruments,
            &eng.policy,
        );
        let experiment_id = experiment["experiment_id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let result_hash = crate::experiment::result_hash(&eng.trades, &eng.equity_curve, &metrics);
        let summary = crate::experiment::build_summary(
            &experiment_id,
            &result_hash,
            &eng.compiled,
            &metrics,
            cfg,
            base_secs,
        );

        let orders_final: Vec<Order> = eng.book.values().cloned().collect();
        Ok(RunResult {
            trades: eng.trades,
            orders: orders_final,
            fills: eng.fills,
            equity_curve: eng.equity_curve,
            ledger: eng.account.ledger,
            metrics,
            monte_carlo: mc,
            experiment,
            summary,
            experiment_id,
            result_hash,
        })
    }
}

struct Engine<'a> {
    compiled: CompiledStrategy,
    cfg: &'a EngineConfig,
    base_secs: i64,
    signals: &'a BTreeMap<String, Vec<(Ts, D)>>,
    actions: &'a [bt_data::actions::CorporateAction],
    policy: AmbiguityPolicy,
    instruments: BTreeMap<String, Instrument>,
    costs: CostModels,
    account: Account,
    book: BTreeMap<u64, Order>,
    fills: Vec<FillRecord>,
    trades: Vec<TradeRecord>,
    equity_curve: Vec<EquityPoint>,
    planned: BTreeMap<u64, (Option<D>, Option<D>)>,
    states: BTreeMap<String, SymbolState<'a>>,
    ids: IdGenerator,
    trade_ids: IdGenerator,
    entry_fills_today: u64,
    peak_equity: D,
    day_start_equity: D,
    current_day: Option<chrono::NaiveDate>,
    sink: &'a mut dyn EventSink,
    seq: Sequencer,
    engine_version: String,
    /// Volume of the bar currently being processed (consumed by fills).
    fill_volume: Option<D>,
}

impl<'a> Engine<'a> {
    fn emit(&mut self, ts: Ts, event: Event) {
        let s = self.seq.next_seq();
        self.sink.record(EventRecord { seq: s, ts, event });
    }

    fn cancel_protective(&mut self, symbol: &str, ts: Ts, reason: &str) {
        let ids = self
            .states
            .get_mut(symbol)
            .map(|s| s.protective.drain(..).collect::<Vec<_>>());
        if let Some(ids) = ids {
            for oid in ids {
                let status = self.book.get(&oid).map(|o| o.status);
                if status == Some(OrderStatus::Filled) {
                    continue; // the leg that triggered: already filled, never cancelled
                }
                if let Some(o) = self.book.get_mut(&oid) {
                    o.status = OrderStatus::Cancelled;
                }
                self.emit(
                    ts,
                    Event::OrderCancelled {
                        order_id: oid,
                        reason: reason.to_string(),
                    },
                );
            }
        }
    }

    /// Execute one fill of `fill_qty` against the order (entries, exits,
    /// reversals, end-of-data). Partial fills leave the order working with
    /// status `PartiallyFilled`.
    fn execute_fill(
        &mut self,
        order_id: u64,
        fill_qty: D,
        raw_price: D,
        ts: Ts,
        kind: Option<&'static str>,
    ) -> CoreResult<()> {
        let order = self
            .book
            .get_mut(&order_id)
            .ok_or_else(|| CoreError::InvalidOrder(format!("order {order_id} not in book")))?;
        if order.remaining_qty <= dec!(0) {
            return Ok(());
        }
        let side = order.side;
        let qty = fill_qty.min(order.remaining_qty);
        if qty <= dec!(0) {
            return Ok(());
        }
        let symbol = order.symbol.clone();
        let _reason = order.reason;
        let cs = self.states[&symbol].instrument.contract_size;

        let breakdown = build_fill(side, raw_price, qty, cs, &self.costs, self.fill_volume)?;
        {
            let order = self.book.get_mut(&order_id).unwrap();
            order.filled_qty += qty;
            order.remaining_qty -= qty;
            order.status = if order.remaining_qty <= dec!(0) {
                OrderStatus::Filled
            } else {
                OrderStatus::PartiallyFilled
            };
            // running average fill price across partials
            let prev = order.avg_fill_price.unwrap_or(dec!(0));
            let prev_qty = order.filled_qty - qty;
            order.avg_fill_price = Some(if order.filled_qty > dec!(0) {
                (prev * prev_qty + breakdown.fill_price * qty) / order.filled_qty
            } else {
                breakdown.fill_price
            });
            order.commission += breakdown.commission;
            order.slippage += breakdown.slippage_cost + breakdown.spread_cost;
        }

        let fill_id = self.ids.next_id();
        let dir = match side {
            Side::Buy => "long",
            Side::Sell => "short",
        };
        self.fills.push(FillRecord {
            fill_id,
            order_id,
            ts,
            symbol: symbol.clone(),
            side: side.as_str().to_string(),
            quantity: qty,
            raw_price: breakdown.raw_price,
            fill_price: breakdown.fill_price,
            commission: breakdown.commission,
            spread_cost: breakdown.spread_cost,
            slippage_cost: breakdown.slippage_cost,
            notional: breakdown.notional,
            reason: kind.unwrap_or("market").to_string(),
        });
        self.emit(
            ts,
            Event::OrderFilled {
                order_id,
                fill_id,
                price: breakdown.fill_price,
                raw_price: breakdown.raw_price,
                quantity: qty,
                commission: breakdown.commission,
                spread_cost: breakdown.spread_cost,
                slippage_cost: breakdown.slippage_cost,
            },
        );

        let (sl_dist, tp_dist) = self.planned.get(&order_id).copied().unwrap_or((None, None));
        // Protective levels anchor to the RAW market price (documented) so
        // risk distance is cost-free and triggers compare against market bars.
        let stop_price = sl_dist.map(|d| match side {
            Side::Buy => breakdown.raw_price - d,
            Side::Sell => breakdown.raw_price + d,
        });
        let target_price = tp_dist.map(|d| match side {
            Side::Buy => breakdown.raw_price + d,
            Side::Sell => breakdown.raw_price - d,
        });

        let outcome = self.account.apply_fill(
            order_id,
            &symbol,
            side,
            qty,
            breakdown.raw_price,
            breakdown.fill_price,
            breakdown.commission,
            breakdown.spread_cost,
            breakdown.slippage_cost,
            cs,
            ts,
            dir,
            stop_price,
            stop_price,
            target_price,
            kind.unwrap_or("entry"),
        )?;

        let position_after = self
            .account
            .positions
            .get(&symbol)
            .map(|p| (p.qty, p.avg_entry, p.initial_risk, p.direction.clone()));

        // --- trade builder lifecycle ------------------------------------------
        match &outcome {
            ApplyOutcome::Opened => {
                self.entry_fills_today += 1;
                let risk = position_after.as_ref().and_then(|(_, _, r, _)| *r);
                let b = TradeBuilder::start(
                    self.trade_ids.next_id(),
                    &symbol,
                    dir,
                    ts,
                    breakdown.raw_price,
                    qty,
                    risk,
                    kind.unwrap_or("entry"),
                    breakdown.commission,
                    breakdown.spread_cost,
                    breakdown.slippage_cost,
                );
                self.states.get_mut(&symbol).unwrap().trade = Some(b);
                self.spawn_protective(&symbol, side, qty, (stop_price, target_price), order_id, ts);
            }
            ApplyOutcome::Increased => {
                self.entry_fills_today += 1;
                if let Some(b) = self.states.get_mut(&symbol).unwrap().trade.as_mut() {
                    b.add_exit_fees(
                        breakdown.commission,
                        breakdown.spread_cost,
                        breakdown.slippage_cost,
                    );
                    b.grow_peak(qty);
                }
            }
            ApplyOutcome::Reduced { realized, .. } => {
                if let Some(b) = self.states.get_mut(&symbol).unwrap().trade.as_mut() {
                    b.record_exit(ts, breakdown.raw_price, qty, *realized);
                    b.add_exit_fees(
                        breakdown.commission,
                        breakdown.spread_cost,
                        breakdown.slippage_cost,
                    );
                }
            }
            ApplyOutcome::Closed { realized } => {
                if let Some(mut b) = self.states.get_mut(&symbol).unwrap().trade.take() {
                    b.record_exit(ts, breakdown.raw_price, qty, *realized);
                    b.add_exit_fees(
                        breakdown.commission,
                        breakdown.spread_cost,
                        breakdown.slippage_cost,
                    );
                    if let Ok(rec) = b.finish(kind.unwrap_or("exit"), true) {
                        self.trades.push(rec);
                    }
                }
                self.cancel_protective(&symbol, ts, "position closed");
            }
            ApplyOutcome::Reversed { realized, new_qty } => {
                self.entry_fills_today += 1;
                // A reversal fill is the entry of the new trade: the fill's
                // costs belong to the NEW builder (documented).
                if let Some(mut b) = self.states.get_mut(&symbol).unwrap().trade.take() {
                    b.record_exit(ts, breakdown.raw_price, new_qty.abs(), *realized);
                    if let Ok(rec) = b.finish("reversal", true) {
                        self.trades.push(rec);
                    }
                }
                self.cancel_protective(&symbol, ts, "position reversed");
                if let Some((q, _, risk, direction)) = &position_after {
                    let b = TradeBuilder::start(
                        self.trade_ids.next_id(),
                        &symbol,
                        direction,
                        ts,
                        breakdown.raw_price,
                        q.abs(),
                        *risk,
                        kind.unwrap_or("entry"),
                        breakdown.commission,
                        breakdown.spread_cost,
                        breakdown.slippage_cost,
                    );
                    self.states.get_mut(&symbol).unwrap().trade = Some(b);
                    self.spawn_protective(
                        &symbol,
                        side,
                        q.abs(),
                        (stop_price, target_price),
                        order_id,
                        ts,
                    );
                }
            }
        }

        // --- position lifecycle events ------------------------------------------
        match &outcome {
            ApplyOutcome::Opened => {
                self.emit(
                    ts,
                    Event::PositionOpened {
                        symbol: symbol.clone(),
                        quantity: position_after
                            .as_ref()
                            .map(|(q, _, _, _)| *q)
                            .unwrap_or(dec!(0)),
                        avg_price: position_after
                            .as_ref()
                            .map(|(_, a, _, _)| *a)
                            .unwrap_or(dec!(0)),
                        stop: stop_price,
                        target: target_price,
                    },
                );
            }
            ApplyOutcome::Increased => {
                if let Some((q, a, _, _)) = position_after {
                    self.emit(
                        ts,
                        Event::PositionIncreased {
                            symbol: symbol.clone(),
                            quantity: q,
                            avg_price: a,
                        },
                    );
                }
            }
            ApplyOutcome::Reduced { realized, .. } => {
                self.emit(
                    ts,
                    Event::PositionReduced {
                        symbol: symbol.clone(),
                        realized_pnl: *realized,
                    },
                );
            }
            ApplyOutcome::Closed { realized } => {
                self.emit(
                    ts,
                    Event::PositionClosed {
                        symbol: symbol.clone(),
                        realized_pnl: *realized,
                        reason: kind.unwrap_or("exit").to_string(),
                    },
                );
            }
            ApplyOutcome::Reversed { realized, new_qty } => {
                self.emit(
                    ts,
                    Event::PositionReversed {
                        symbol: symbol.clone(),
                        new_quantity: *new_qty,
                    },
                );
                self.emit(
                    ts,
                    Event::PositionClosed {
                        symbol: symbol.clone(),
                        realized_pnl: *realized,
                        reason: "reversal".to_string(),
                    },
                );
            }
        }
        Ok(())
    }

    /// Create SL/TP protective orders for a freshly opened/reversed position.
    fn spawn_protective(
        &mut self,
        symbol: &str,
        side: Side,
        qty: D,
        levels: (Option<D>, Option<D>),
        parent_id: u64,
        ts: Ts,
    ) {
        let (stop_price, target_price) = levels;
        if let Some(sp) = stop_price {
            let oid = self.ids.next_id();
            let mut o = Order::new(
                oid,
                "strategy",
                symbol,
                side.opposite(),
                OrderType::Stop,
                qty,
                None,
                Some(sp),
                ts,
                OrderReason::StopLoss,
                PositionEffect::Close,
                Some(parent_id),
            );
            o.status = OrderStatus::Active;
            o.activation_ts = Some(ts);
            self.book.insert(oid, o);
            self.states.get_mut(symbol).unwrap().protective.push(oid);
        }
        if let Some(tp) = target_price {
            let oid = self.ids.next_id();
            let mut o = Order::new(
                oid,
                "strategy",
                symbol,
                side.opposite(),
                OrderType::Limit,
                qty,
                Some(tp),
                None,
                ts,
                OrderReason::TakeProfit,
                PositionEffect::Close,
                Some(parent_id),
            );
            o.status = OrderStatus::Active;
            o.activation_ts = Some(ts);
            self.book.insert(oid, o);
            self.states.get_mut(symbol).unwrap().protective.push(oid);
        }
    }

    /// Create + accept a market order and fill it now or queue for next open.
    #[allow(clippy::too_many_arguments)]
    /// Submit an entry order (market, limit, stop or stop-limit) and either
    /// fill it immediately (current_close market) or let it rest working.
    #[allow(clippy::too_many_arguments)]
    fn submit_entry_order(
        &mut self,
        symbol: &str,
        side: Side,
        qty: D,
        entry: &EntryOrderDef,
        prices: &EntryPrices,
        reason: OrderReason,
        effect: PositionEffect,
        decision_ts: Ts,
        distances: (Option<D>, Option<D>),
    ) -> CoreResult<()> {
        let (order_type, limit_price, stop_price) = match entry {
            EntryOrderDef::Market => (OrderType::Market, None, None),
            EntryOrderDef::Limit { .. } => (OrderType::Limit, prices.limit_price, None),
            EntryOrderDef::Stop { .. } => (OrderType::Stop, None, prices.stop_price),
            EntryOrderDef::StopLimit { .. } => {
                (OrderType::StopLimit, prices.limit_price, prices.stop_price)
            }
        };
        let oid = self.ids.next_id();
        let mut o = Order::new(
            oid,
            "strategy",
            symbol,
            side,
            order_type,
            qty,
            limit_price,
            stop_price,
            decision_ts,
            reason,
            effect,
            None,
        );
        o.status = OrderStatus::Accepted;
        self.book.insert(oid, o);
        self.emit(
            decision_ts,
            Event::OrderCreated {
                order_id: oid,
                symbol: symbol.to_string(),
                side: side.as_str().into(),
                order_type: format!("{order_type:?}").to_lowercase(),
                quantity: qty,
                reason: format!("{reason:?}").to_lowercase(),
            },
        );
        self.emit(decision_ts, Event::OrderAccepted { order_id: oid });
        self.planned.insert(oid, distances);

        let is_market = matches!(entry, EntryOrderDef::Market);
        if is_market && self.cfg.execution.model == ExecutionModel::CurrentClose {
            // Fill at the decision bar's close; the participation cap may
            // partial-fill, remainder re-queues as a working order.
            let base_secs = self.base_secs;
            let bar_idx = self.states[symbol]
                .base
                .bars
                .iter()
                .position(|b| b.close_time(base_secs) == decision_ts);
            let close = match bar_idx {
                Some(i) => self.states[symbol].base.bars[i].close,
                None => self.states[symbol]
                    .base
                    .bars
                    .last()
                    .map(|b| b.close)
                    .unwrap_or(dec!(1)),
            };
            let capped = self.capped_entry_qty(qty);
            self.execute_fill(oid, capped, close, decision_ts, None)?;
            self.requeue_remainder(symbol, oid);
        } else {
            self.states.get_mut(symbol).unwrap().pending.push_back(oid);
        }
        Ok(())
    }

    /// Apply corporate actions effective at `ts` for `symbol`:
    /// - dividend: cash to balance (long receives, short pays) + trade record
    /// - split: quantity/price adjustment across position, builder, protective
    ///   orders and planned distances. Economic values are invariant.
    fn apply_corporate_actions(&mut self, ts: Ts, symbol: &str) -> CoreResult<()> {
        let actions: Vec<bt_data::actions::CorporateAction> = self
            .actions
            .iter()
            .filter(|a| a.ts == ts && a.symbol == symbol)
            .cloned()
            .collect();
        for action in actions {
            match action.kind {
                bt_data::actions::CorporateActionKind::Dividend { amount } => {
                    let cs = self.states[symbol].instrument.contract_size;
                    let cash = self.account.apply_dividend(symbol, amount, cs, ts)?;
                    if cash != dec!(0) {
                        if let Some(b) = self.states.get_mut(symbol).unwrap().trade.as_mut() {
                            b.add_dividend(cash);
                        }
                        self.emit(
                            ts,
                            Event::FinancingApplied {
                                symbol: symbol.to_string(),
                                amount: -cash, // positive cash = negative cost
                            },
                        );
                    }
                }
                bt_data::actions::CorporateActionKind::Split { ratio } => {
                    let had_position = self.account.apply_split(symbol, ratio)?;
                    // adjust protective order price levels
                    let protective = self.states[symbol].protective.clone();
                    for oid in protective {
                        if let Some(o) = self.book.get_mut(&oid) {
                            if let Some(s) = o.stop_price.as_mut() {
                                *s /= ratio;
                            }
                            if let Some(l) = o.limit_price.as_mut() {
                                *l /= ratio;
                            }
                        }
                    }
                    // scale pending order quantities and their price levels
                    let pending: Vec<u64> = self
                        .states
                        .get_mut(symbol)
                        .unwrap()
                        .pending
                        .iter()
                        .copied()
                        .collect();
                    for oid in pending {
                        if let Some(o) = self.book.get_mut(&oid) {
                            o.quantity *= ratio;
                            o.remaining_qty *= ratio;
                            if let Some(s) = o.stop_price.as_mut() {
                                *s /= ratio;
                            }
                            if let Some(l) = o.limit_price.as_mut() {
                                *l /= ratio;
                            }
                        }
                    }
                    // planned distances are price distances: divide by ratio
                    for dists in self.planned.values_mut() {
                        if let Some(d) = dists.0.as_mut() {
                            *d /= ratio;
                        }
                        if let Some(d) = dists.1.as_mut() {
                            *d /= ratio;
                        }
                    }
                    // trade builder: entry price divides, quantities multiply
                    if let Some(b) = self.states.get_mut(symbol).unwrap().trade.as_mut() {
                        b.apply_split(ratio);
                    }
                    if had_position {
                        self.emit(
                            ts,
                            Event::PositionIncreased {
                                symbol: symbol.to_string(),
                                quantity: self
                                    .account
                                    .positions
                                    .get(symbol)
                                    .map(|p| p.qty)
                                    .unwrap_or(dec!(0)),
                                avg_price: self
                                    .account
                                    .positions
                                    .get(symbol)
                                    .map(|p| p.avg_entry)
                                    .unwrap_or(dec!(0)),
                            },
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// Participation-capped quantity for ENTRY fills (cap not set => full qty).
    fn capped_entry_qty(&self, qty: D) -> D {
        match (self.cfg.execution.participation_cap, self.fill_volume) {
            (Some(cap), Some(vol)) if vol > dec!(0) => {
                let max_qty = cap * vol;
                if qty > max_qty {
                    max_qty
                } else {
                    qty
                }
            }
            _ => qty,
        }
    }

    /// After a (possibly partial) entry fill, re-queue any remainder so it
    /// keeps working on later bars.
    fn requeue_remainder(&mut self, symbol: &str, order_id: u64) {
        let (remaining, status) = {
            let o = &self.book[&order_id];
            (o.remaining_qty, o.status)
        };
        if remaining > dec!(0) && status != OrderStatus::Filled {
            self.states
                .get_mut(symbol)
                .unwrap()
                .pending
                .push_back(order_id);
        }
    }

    fn process_bar(&mut self, ts: Ts, symbol: &str, bar_idx: usize) -> CoreResult<()> {
        let base_secs = self.base_secs;
        let bar: Bar = self.states[symbol].base.bars[bar_idx].clone();
        self.fill_volume = bar.volume;
        let cs = self.states[symbol].instrument.contract_size;

        self.emit(
            ts,
            Event::MarketEvent {
                symbol: symbol.to_string(),
                open: bar.open,
                high: bar.high,
                low: bar.low,
                close: bar.close,
                volume: bar.volume,
            },
        );

        // 1.5 corporate actions on the ex-date bar (holder entering the bar):
        //     dividends credit cash; splits adjust quantities and levels.
        self.apply_corporate_actions(ts, symbol)?;

        // 2. working orders: market fills at the open; limit/stop/stop-limit
        //    trigger intrabar against this bar's range (gap-aware).
        let span = BarSpan {
            open: bar.open,
            high: bar.high,
            low: bar.low,
            close: bar.close,
        };
        let pending: Vec<u64> = self
            .states
            .get_mut(symbol)
            .unwrap()
            .pending
            .drain(..)
            .collect();
        for oid in pending {
            let (otype, limit_px, stop_px) = {
                let o = &self.book[&oid];
                (o.order_type, o.limit_price, o.stop_price)
            };
            let strict = self.cfg.execution.strict_trigger;
            let trigger: Option<D> = match otype {
                OrderType::Market => Some(bar.open),
                OrderType::Limit => {
                    let buy = self.book[&oid].side == Side::Buy;
                    let Some(px) = limit_px else {
                        return Err(CoreError::InvalidOrder(format!(
                            "limit order {oid} without a limit price"
                        )));
                    };
                    bt_execution::intrabar::limit_trigger(buy, px, span, strict)
                }
                OrderType::Stop => {
                    let sell = self.book[&oid].side == Side::Sell;
                    let Some(px) = stop_px else {
                        return Err(CoreError::InvalidOrder(format!(
                            "stop order {oid} without a stop price"
                        )));
                    };
                    bt_execution::intrabar::stop_trigger(sell, px, span, strict)
                }
                OrderType::StopLimit => {
                    let sell = self.book[&oid].side == Side::Sell;
                    let (Some(stop), Some(limit)) = (stop_px, limit_px) else {
                        return Err(CoreError::InvalidOrder(format!(
                            "stop-limit order {oid} missing prices"
                        )));
                    };
                    let armed = bt_execution::intrabar::stop_trigger(sell, stop, span, strict);
                    if armed.is_some() {
                        // Once the stop is touched, the limit governs the fill.
                        bt_execution::intrabar::limit_trigger(
                            self.book[&oid].side == Side::Buy,
                            limit,
                            span,
                            strict,
                        )
                    } else {
                        None
                    }
                }
            };
            let Some(raw_price) = trigger else {
                // Order keeps working.
                self.states.get_mut(symbol).unwrap().pending.push_back(oid);
                continue;
            };
            let is_entry = matches!(self.book[&oid].reason, OrderReason::StrategyEntry);
            let remaining = self.book[&oid].remaining_qty;
            let fill_qty = if is_entry {
                self.capped_entry_qty(remaining)
            } else {
                remaining
            };
            if fill_qty <= dec!(0) {
                self.states.get_mut(symbol).unwrap().pending.push_back(oid);
                continue;
            }
            self.execute_fill(oid, fill_qty, raw_price, ts, None)?;
            self.requeue_remainder(symbol, oid);
        }

        // 3. intrabar protective triggers
        let pos_snapshot = self
            .account
            .positions
            .get(symbol)
            .map(|p| (p.stop, p.target, p.trailing_stop, p.is_long()));
        if let Some((stop, target, trailing, long)) = pos_snapshot {
            if stop.is_some() || target.is_some() || trailing.is_some() {
                let triggers = ExitTriggers {
                    stop_loss: stop,
                    take_profit: target,
                    trailing_stop: trailing,
                };
                let span = BarSpan {
                    open: bar.open,
                    high: bar.high,
                    low: bar.low,
                    close: bar.close,
                };
                match evaluate_exit(
                    long,
                    &triggers,
                    span,
                    self.cfg.execution.strict_trigger,
                    &self.policy,
                ) {
                    ExitDecision::None => {}
                    ExitDecision::Ambiguous => {
                        self.emit(
                            ts,
                            Event::AmbiguityDeferred {
                                symbol: symbol.to_string(),
                                detail: "bar range touches both stop-loss and take-profit; \
                                         exit deferred by policy=reject"
                                    .into(),
                            },
                        );
                    }
                    ExitDecision::Exit(kind, raw_price) => {
                        let protective = self.states[symbol].protective.clone();
                        if let Some(oid) = protective.first().copied() {
                            // cancel the remaining protective orders (the other leg)
                            for other in protective.iter().skip(1) {
                                if let Some(o) = self.book.get_mut(other) {
                                    o.status = OrderStatus::Cancelled;
                                }
                                self.emit(
                                    ts,
                                    Event::OrderCancelled {
                                        order_id: *other,
                                        reason: "position closed".into(),
                                    },
                                );
                            }
                            // retarget the surviving protective order to the full exit
                            let side = if long { Side::Sell } else { Side::Buy };
                            let qty = self
                                .account
                                .positions
                                .get(symbol)
                                .map(|p| p.qty.abs())
                                .unwrap_or(dec!(0));
                            {
                                let o = self.book.get_mut(&oid).unwrap();
                                o.side = side;
                                o.quantity = qty;
                                o.remaining_qty = qty;
                            }
                            let remaining = self.book[&oid].remaining_qty;
                            self.execute_fill(oid, remaining, raw_price, ts, Some(kind.as_str()))?;
                            self.cancel_protective(symbol, ts, "position closed");
                        }
                    }
                }
            }
        }

        // 4. financing at close
        if let Some(p) = self.account.positions.get(symbol).cloned() {
            let bars_per_day = D::from(86_400i64 / base_secs.max(1));
            let held_days = p.bars_held / u64::try_from(bars_per_day.max(D::from(1))).unwrap_or(1);
            let amount =
                self.costs
                    .financing
                    .per_bar(p.qty, bar.close, cs, bars_per_day, held_days);
            if amount != dec!(0) {
                self.account.apply_financing(symbol, amount, ts)?;
                if let Some(b) = self.states.get_mut(symbol).unwrap().trade.as_mut() {
                    b.add_financing(amount);
                }
                self.emit(
                    ts,
                    Event::FinancingApplied {
                        symbol: symbol.to_string(),
                        amount,
                    },
                );
            }
        }

        // 5. excursions
        if let Some(p) = self.account.positions.get_mut(symbol) {
            p.update_excursions(bar.high, bar.low);
            let (mae, mfe, bars) = (p.mae, p.mfe, p.bars_held);
            if let Some(b) = self.states.get_mut(symbol).unwrap().trade.as_mut() {
                b.observe_excursions(mae, mfe, bars);
            }
        }

        // 6. decision phase ------------------------------------------------------
        let decision_ts = bar.close_time(base_secs);
        {
            let state = self.states.get_mut(symbol).unwrap();
            state.indicators.on_bar_close(None, &bar);
            for (tf_name, tf_secs) in &self.compiled.htfs {
                if decision_ts.timestamp().rem_euclid(*tf_secs) == 0 {
                    let series = state.htfs.get(tf_name).unwrap();
                    let bucket = floor_to_interval(bar.open_time, *tf_secs);
                    let cursor = state.htf_cursor.entry(tf_name.clone()).or_insert(0);
                    if let Some(found) = series.bars[*cursor..]
                        .iter()
                        .position(|b| b.open_time == bucket)
                    {
                        let i = *cursor + found;
                        let htf_bar = series.bars[i].clone();
                        state.indicators.on_bar_close(Some(tf_name), &htf_bar);
                        *cursor = i + 1;
                    }
                }
            }
        }

        // eval context (all immutable borrows)
        let snapshot = self.mark_snapshot(ts, bar_idx)?;
        let position_view = self.account.positions.get(symbol).map(|p| {
            let unrealized = p.unrealized(bar.close, cs);
            let eq = self.account.balance + unrealized;
            PositionView {
                is_long: p.is_long(),
                bars_held: p.bars_held,
                unrealized,
                unrealized_pct: if eq.is_zero() {
                    dec!(0)
                } else {
                    unrealized / eq * dec!(100)
                },
            }
        });
        let drawdown_pct = if self.peak_equity.is_zero() {
            dec!(0)
        } else {
            (self.peak_equity - snapshot.equity) / self.peak_equity * dec!(100)
        };
        let account_view = AccountView {
            equity: snapshot.equity,
            balance: snapshot.balance,
            drawdown_pct,
        };
        let intent = {
            let state = self.states.get(symbol).unwrap();
            let base_access = BarAccess::new(&state.base.bars, bar_idx + 1);
            let mut htf_access: BTreeMap<String, BarAccess> = BTreeMap::new();
            for (tf_name, series) in &state.htfs {
                let cursor = state.htf_cursor.get(tf_name).copied().unwrap_or(0);
                htf_access.insert(tf_name.clone(), BarAccess::new(&series.bars, cursor));
            }
            // Cross-symbol access: every strategy symbol, cursor = bars whose
            // open_time <= ts (same-grid bars close at the same decision ts).
            let mut symbol_access: BTreeMap<String, BarAccess> = BTreeMap::new();
            for (sym, st) in &self.states {
                let cursor = st.base.bars.partition_point(|b| b.open_time <= ts);
                symbol_access.insert(sym.clone(), BarAccess::new(&st.base.bars, cursor));
            }
            let rt = &state.indicators;
            let ind_fn = |k: &str, o: usize| rt.value(k, o);
            let ctx = EvalContext {
                decision_ts,
                base_bars: &base_access,
                htf_bars: &htf_access,
                symbol_bars: &symbol_access,
                ind: &ind_fn,
                position: position_view,
                account: account_view,
                signals: self.signals,
            };
            let intent = eval_intent(&self.compiled, &ctx);
            // Resolve entry trigger prices at decision time (same context).
            let prices = self.entry_prices_from_ctx(&ctx);
            (intent, prices)
        };
        let (intent, prices) = intent;
        let entry_prices = prices?;
        if let Some(p) = self.account.positions.get_mut(symbol) {
            p.bars_held += 1;
        }

        match intent {
            Intent::None => {}
            Intent::Exit => {
                if let Some(p) = self.account.positions.get(symbol).cloned() {
                    self.emit(
                        decision_ts,
                        Event::StrategySignal {
                            action: "exit".into(),
                            detail: symbol.to_string(),
                        },
                    );
                    let side = if p.is_long() { Side::Sell } else { Side::Buy };
                    self.submit_entry_order(
                        symbol,
                        side,
                        p.qty.abs(),
                        &EntryOrderDef::Market,
                        &EntryPrices::default(),
                        OrderReason::StrategyExit,
                        PositionEffect::Close,
                        decision_ts,
                        (None, None),
                    )?;
                }
            }
            Intent::EnterLong | Intent::EnterShort => {
                let want_long = intent == Intent::EnterLong;
                self.emit(
                    decision_ts,
                    Event::StrategySignal {
                        action: if want_long {
                            "buy".into()
                        } else {
                            "sell".into()
                        },
                        detail: symbol.to_string(),
                    },
                );
                let side = if want_long { Side::Buy } else { Side::Sell };

                // protective distances at decision time
                let tp_def = self.compiled.take_profit.clone();
                let sl_dist = self.stop_distance_at(symbol)?;
                let tp_dist = match &tp_def {
                    TakeProfitDef::None => None,
                    TakeProfitDef::FixedDistance { value } => Some(*value),
                    TakeProfitDef::RiskMultiple { value } => sl_dist.map(|d| d * *value),
                };

                // risk sizing
                let sizing_mode = self.compiled.sizing.clone();
                let atr_value = match &sizing_mode {
                    SizingMode::AtrBased { atr_period, .. } => {
                        let key = format!("base|atr({atr_period})|ohlc");
                        self.indicator_num(symbol, &key)?
                    }
                    _ => None,
                };

                let existing = self.account.positions.get(symbol);
                let existing_qty = existing.map(|p| p.qty).unwrap_or(dec!(0));
                let is_reversal = existing
                    .map(|p| (p.is_long() && !want_long) || (!p.is_long() && want_long))
                    .unwrap_or(false);
                let existing_notional = existing
                    .map(|p| p.qty.abs() * bar.close * cs)
                    .unwrap_or(dec!(0));
                let total_notional: D = self
                    .account
                    .positions
                    .values()
                    .map(|p| {
                        let cs_p = self
                            .states
                            .get(&p.symbol)
                            .map(|st| st.instrument.contract_size)
                            .unwrap_or(dec!(1));
                        p.qty.abs() * bar.close * cs_p
                    })
                    .sum();
                let sizing_ctx = RiskContext {
                    equity: snapshot.equity,
                    peak_equity: self.peak_equity,
                    day_start_equity: self.day_start_equity,
                    open_position_count: self.account.positions.len(),
                    symbol_qty: if is_reversal { dec!(0) } else { existing_qty },
                    symbol_notional: if is_reversal {
                        dec!(0)
                    } else {
                        existing_notional
                    },
                    total_notional,
                    is_reversal,
                    entries_in_position: if is_reversal {
                        0
                    } else {
                        existing.map(|p| p.entries).unwrap_or(0)
                    },
                    entry_fills_today: self.entry_fills_today,
                    price: bar.close,
                    contract_size: cs,
                    leverage: self.states[symbol].instrument.leverage,
                };
                let sized = self_sizing(
                    &RiskEngine::new(self.cfg.risk.clone()),
                    &self.compiled.sizing,
                    &sizing_ctx,
                    sl_dist,
                    atr_value,
                    &self.states[symbol].instrument,
                );
                let qty = match sized {
                    Ok(q) => q,
                    Err(e) => {
                        self.emit(
                            decision_ts,
                            Event::RiskRejected {
                                reason: "sizing_error".into(),
                                detail: e.to_string(),
                            },
                        );
                        return Ok(());
                    }
                };
                // A reversal must close the existing position AND open the new
                // one: in a netting book the order quantity is existing + new
                // (an equal-size opposing order would merely close).
                let mut final_qty = qty;
                if let Some(p) = self.account.positions.get(symbol) {
                    let opposing = (p.is_long() && !want_long) || (!p.is_long() && want_long);
                    if opposing && final_qty < p.qty.abs() + qty {
                        final_qty = p.qty.abs() + qty;
                    }
                }
                let risk_engine = RiskEngine::new(self.cfg.risk.clone());
                match risk_engine.check_entry(&sizing_ctx, final_qty) {
                    RiskDecision::Rejected(reason) => {
                        self.emit(
                            decision_ts,
                            Event::RiskRejected {
                                reason: "entry_blocked".into(),
                                detail: reason,
                            },
                        );
                    }
                    RiskDecision::Approved(q) => {
                        let entry_def = self.compiled.entry.clone();
                        self.submit_entry_order(
                            symbol,
                            side,
                            q,
                            &entry_def,
                            &entry_prices,
                            OrderReason::StrategyEntry,
                            PositionEffect::Open,
                            decision_ts,
                            (sl_dist, tp_dist),
                        )?;
                    }
                }
            }
        }

        // 7. trailing stop update at close
        let trail_def = self.compiled.trailing_stop.clone();
        if let Some(trail) = &trail_def {
            let dist = match trail {
                TrailingDef::FixedDistance { value } => Some(*value),
                TrailingDef::AtrMultiple { period, multiple } => {
                    let key = format!("base|atr({period})|ohlc");
                    self.indicator_num(symbol, &key)?.map(|a| a * *multiple)
                }
            };
            if let Some(d) = dist {
                if let Some(p) = self.account.positions.get_mut(symbol) {
                    let candidate = if p.is_long() {
                        bar.close - d
                    } else {
                        bar.close + d
                    };
                    let better = match (p.is_long(), p.trailing_stop) {
                        (true, None) => true,
                        (true, Some(cur)) => candidate > cur,
                        (false, None) => true,
                        (false, Some(cur)) => candidate < cur,
                    };
                    if better {
                        p.trailing_stop = Some(candidate);
                        self.emit(
                            decision_ts,
                            Event::TrailingStopUpdated {
                                symbol: symbol.to_string(),
                                level: candidate,
                            },
                        );
                    }
                }
            }
        }

        // 8. margin check
        if snapshot.free_margin < dec!(0) {
            self.emit(
                decision_ts,
                Event::MarginViolation {
                    detail: format!("free_margin {} < 0", snapshot.free_margin),
                },
            );
        }
        Ok(())
    }

    /// Evaluate the compiled entry order's trigger prices at decision time.
    /// Errors (NA) on expression prices are returned as an error: a resting
    /// order with an unknown price would be a silent correctness hazard.
    fn entry_prices_from_ctx(&self, ctx: &EvalContext) -> CoreResult<EntryPrices> {
        let eval_price = |spec: &PriceSpec| -> CoreResult<Option<D>> {
            match spec {
                PriceSpec::Fixed(v) => Ok(Some(*v)),
                PriceSpec::Expr { expr, .. } => match expr.eval(ctx, 0) {
                    Value::Num(n) => Ok(Some(n)),
                    _ => Err(CoreError::InvalidOrder(
                        "entry price expression evaluated to NA at decision time".into(),
                    )),
                },
            }
        };
        Ok(match &self.compiled.entry {
            EntryOrderDef::Market => EntryPrices::default(),
            EntryOrderDef::Limit { price } => EntryPrices {
                limit_price: eval_price(price)?,
                stop_price: None,
            },
            EntryOrderDef::Stop { price } => EntryPrices {
                limit_price: None,
                stop_price: eval_price(price)?,
            },
            EntryOrderDef::StopLimit {
                stop_price,
                limit_price,
            } => EntryPrices {
                limit_price: eval_price(limit_price)?,
                stop_price: eval_price(stop_price)?,
            },
        })
    }

    fn indicator_num(&mut self, symbol: &str, key: &str) -> CoreResult<Option<D>> {
        Ok(
            match self.states.get(symbol).unwrap().indicators.value(key, 0) {
                Value::Num(n) => Some(n),
                _ => None,
            },
        )
    }

    fn stop_distance_at(&mut self, symbol: &str) -> CoreResult<Option<D>> {
        let def = self.compiled.stop_loss.clone();
        Ok(match &def {
            StopLossDef::None => None,
            StopLossDef::FixedDistance { value } => Some(*value),
            StopLossDef::AtrMultiple { period, multiple } => {
                let key = format!("base|atr({period})|ohlc");
                self.indicator_num(symbol, &key)?.map(|a| a * *multiple)
            }
        })
    }

    /// Account snapshot using each symbol's current/last known close.
    fn mark_snapshot(
        &self,
        ts: Ts,
        _bar_idx: usize,
    ) -> CoreResult<bt_accounting::account::AccountSnapshot> {
        let mut marks = BTreeMap::new();
        for (s, st) in &self.states {
            let mark = st
                .base
                .bars
                .iter()
                .rfind(|b| b.open_time <= ts)
                .map(|b| b.close)
                .unwrap_or(dec!(1));
            marks.insert(s.clone(), mark);
        }
        self.account.snapshot(&marks, &self.instruments)
    }

    fn snapshot(&mut self, ts: Ts) -> CoreResult<()> {
        let snap = self.mark_snapshot(ts, 0)?;
        let date = ts.date_naive();
        if self.current_day != Some(date) {
            self.current_day = Some(date);
            self.day_start_equity = snap.equity;
            self.entry_fills_today = 0;
        }
        if snap.equity > self.peak_equity {
            self.peak_equity = snap.equity;
        }
        let dd = self.peak_equity - snap.equity;
        let dd_pct = if self.peak_equity.is_zero() {
            dec!(0)
        } else {
            dd / self.peak_equity * dec!(100)
        };
        let in_position = !self.account.positions.is_empty();
        self.equity_curve.push(EquityPoint {
            ts,
            equity: snap.equity,
            balance: snap.balance,
            unrealized: snap.unrealized,
            drawdown: dd,
            drawdown_pct: dd_pct,
            in_position,
        });
        self.emit(
            ts,
            Event::EquitySnapshot {
                equity: snap.equity,
                drawdown: dd,
                drawdown_pct: dd_pct,
                in_position,
            },
        );
        Ok(())
    }

    fn finish_data(&mut self) -> CoreResult<()> {
        match self.cfg.runtime.end_policy {
            EndPolicy::CloseAll => {
                let symbols: Vec<String> = self.states.keys().cloned().collect();
                for symbol in symbols {
                    let Some(p) = self.account.positions.get(&symbol).cloned() else {
                        continue;
                    };
                    let last_ts = self.states[&symbol]
                        .base
                        .bars
                        .last()
                        .map(|b| b.open_time)
                        .unwrap_or(Ts::UNIX_EPOCH);
                    let last_close = self.states[&symbol]
                        .base
                        .bars
                        .last()
                        .map(|b| b.close)
                        .unwrap_or(dec!(1));
                    let side = if p.is_long() { Side::Sell } else { Side::Buy };
                    let oid = self.ids.next_id();
                    let mut o = Order::new(
                        oid,
                        "strategy",
                        &symbol,
                        side,
                        OrderType::Market,
                        p.qty.abs(),
                        None,
                        None,
                        last_ts,
                        OrderReason::EndOfData,
                        PositionEffect::Close,
                        None,
                    );
                    o.status = OrderStatus::Accepted;
                    self.book.insert(oid, o);
                    self.emit(
                        last_ts,
                        Event::OrderCreated {
                            order_id: oid,
                            symbol: symbol.clone(),
                            side: side.as_str().into(),
                            order_type: "market".into(),
                            quantity: p.qty.abs(),
                            reason: "end_of_data".into(),
                        },
                    );
                    self.emit(last_ts, Event::OrderAccepted { order_id: oid });
                    let remaining = self.book[&oid].remaining_qty;
                    self.execute_fill(oid, remaining, last_close, last_ts, Some("end_of_data"))?;
                }
            }
            EndPolicy::MarkToMarket => {
                // (no forced closes; snapshot below stays as-is)
                self.emit(
                    self.equity_curve
                        .last()
                        .map(|p| p.ts)
                        .unwrap_or(Ts::UNIX_EPOCH),
                    Event::StrategySignal {
                        action: "mark_to_market".into(),
                        detail: "open positions left open at data end".into(),
                    },
                );
            }
        }
        // CloseAll: the FINAL equity point must reflect the post-close balance
        // (realized P&L and exit costs booked), not the pre-close mark.
        if self.cfg.runtime.end_policy == EndPolicy::CloseAll {
            if let Some(last_ts) = self.equity_curve.last().map(|p| p.ts) {
                let snap = self.mark_snapshot(last_ts, 0)?;
                if snap.equity > self.peak_equity {
                    self.peak_equity = snap.equity;
                }
                let dd = self.peak_equity - snap.equity;
                let dd_pct = if self.peak_equity.is_zero() {
                    dec!(0)
                } else {
                    dd / self.peak_equity * dec!(100)
                };
                if let Some(p) = self.equity_curve.last_mut() {
                    p.equity = snap.equity;
                    p.balance = snap.balance;
                    p.unrealized = snap.unrealized;
                    p.drawdown = dd;
                    p.drawdown_pct = dd_pct;
                    p.in_position = false;
                }
                self.emit(
                    last_ts,
                    Event::EquitySnapshot {
                        equity: snap.equity,
                        drawdown: dd,
                        drawdown_pct: dd_pct,
                        in_position: false,
                    },
                );
            }
        }
        Ok(())
    }
}

// small helper to keep the sizing call site tidy
fn self_sizing(
    engine: &RiskEngine,
    mode: &SizingMode,
    ctx: &RiskContext,
    stop_distance: Option<D>,
    atr_value: Option<D>,
    instrument: &Instrument,
) -> CoreResult<D> {
    engine.size_position(mode, ctx, stop_distance, atr_value, instrument)
}
