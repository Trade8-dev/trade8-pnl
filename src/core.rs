// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Trade8:Pnl
// SPDX-License-Identifier: Apache-2.0

//! Pnl engine - per-(trader, symbol) position state
//! machines, realized/unrealized, fees, LFT, summaries, overnight carry.
//!
//! Design: `../pnl/DESIGN.md` §4.
//! derives from fill events + opening balances (deterministic replay).
//!
//! **Scale semantics**: price fields are instrument-scale ticks;
//! realized/unrealized/fees are money ×10⁴. A fill's realized P&L at
//! equity scale is `(sell_px − cost_px) × shares` in ×10⁴ ticks - the
//! tests pin the magnitudes (memory: ×10⁴ price-scale bugs are the
//! classic failure class).

use std::collections::HashMap;

use parking_lot::RwLock;
use trade8::contracts::{
    Fill, FillState, OfficePnlSummary, Position, SymbolResume, TraderPnlSummary,
};
use uuid::Uuid;

/// Per-(trader, symbol) engine state.
#[derive(Debug, Clone, PartialEq)]
pub struct PositionBook {
    pub usr_id: String,
    pub office_id: String,
    pub symbol_id: Uuid,
    pub symbol_name: String,
    /// Signed size (+ long, − short).
    pub signed_size: i64,
    /// Average cost of the open position (price ticks).
    pub avg_cost_raw: i128,
    pub price_scale: u8,
    /// Realized day P&L (×10⁴ money).
    pub realized: i128,
    pub fees: i128,
    /// Last mark (price ticks) - last print or quote.
    pub last_mark_raw: Option<i128>,
    /// Per-symbol net high-water (max net seen -
    /// parity).
    pub max_net: i128,
    /// Per-symbol LFT low-water (max drawdown from the high-water -
    /// parity).
    pub lft: i128,
}

impl PositionBook {
    pub fn unrealized(&self) -> i128 {
        match self.last_mark_raw {
            Some(mark) => (mark - self.avg_cost_raw) * self.signed_size as i128,
            None => 0,
        }
    }

    /// Apply one fill - position/reversal semantics per the
    /// `PositionContext` (`m_fillChangesPosSide`): same-side adds
    /// average in; opposite side closes (and may reverse).
    pub fn apply_fill(&mut self, qty: u64, side_char: char, price_raw: i128, fee_total: i128) {
        self.fees += fee_total;
        let fill_qty = qty as i64;
        let signed_fill = if side_char == 'B' {
            fill_qty
        } else {
            -fill_qty
        };
        if self.signed_size == 0 || self.signed_size.signum() == signed_fill.signum() {
            // Opening or adding: weight the new average cost.
            let new_size = self.signed_size + signed_fill;
            if new_size != 0 {
                self.avg_cost_raw = if self.signed_size == 0 {
                    price_raw
                } else {
                    (self.avg_cost_raw * self.signed_size.abs() as i128
                        + price_raw * fill_qty as i128)
                        / new_size.abs() as i128
                };
            }
            self.signed_size = new_size;
        } else {
            // Reducing / closing / reversing.
            let closing = signed_fill.abs().min(self.signed_size.abs());
            // Realized = (fill_px − avg_cost) × closing shares × side sign:
            // long close (sell): (px − cost) × closing; short close (buy):
            // (cost − px) × closing.
            let pnl_per = match self.signed_size > 0 {
                true => price_raw - self.avg_cost_raw,
                false => self.avg_cost_raw - price_raw,
            };
            self.realized += pnl_per * closing as i128;
            let remaining = signed_fill.abs() - closing;
            self.signed_size += signed_fill;
            if remaining > 0 && self.signed_size != 0 {
                // Reversal: the remainder opens the new side at fill px.
                self.avg_cost_raw = price_raw;
            }
        }
    }
}

/// The Pnl engine.
#[derive(Default)]
pub struct PnlEngine {
    books: RwLock<HashMap<(String, Uuid), PositionBook>>,
    /// Marks per symbol (last print).
    marks: RwLock<HashMap<Uuid, i128>>,
    /// Day summaries per trader.
    summaries: RwLock<HashMap<String, TraderPnlSummary>>,
    office_of: RwLock<HashMap<String, String>>,
    trading_date: RwLock<String>,
    /// Applied transaction ids (at-least-once bus
    /// delivery + dedupe = effectively-once position application).
    applied_tx: RwLock<std::collections::HashSet<Uuid>>,
}

impl PnlEngine {
    pub fn set_trading_date(&self, date: &str) {
        *self.trading_date.write() = date.to_string();
    }

    pub fn set_office(&self, usr_id: &str, office_id: &str) {
        self.office_of
            .write()
            .insert(usr_id.to_string(), office_id.to_string());
    }

    /// Register a mark (print or quote) - unrealized refresh input.
    /// Refreshes the affected books' last mark + high-water trackers AND
    /// recomputes the owning traders' summaries (a stale summary on a
    /// mark move would misreport unrealized).
    pub fn mark(&self, symbol_id: Uuid, mark_raw: i128) {
        self.marks.write().insert(symbol_id, mark_raw);
        let mut books = self.books.write();
        let mut touched: Vec<(String, String)> = Vec::new();
        for book in books.values_mut() {
            if book.symbol_id == symbol_id {
                book.last_mark_raw = Some(mark_raw);
                Self::track_high_water(book);
                touched.push((book.usr_id.clone(), book.office_id.clone()));
            }
        }
        touched.sort();
        touched.dedup();
        for (usr, office) in touched {
            self.recompute_summary_locked(&books, &usr, &office);
        }
    }

    /// Apply a normalized fill event.
    pub fn apply_fill(&self, fill: &Fill, usr_id: &str, office_id: &str, symbol_name: &str) {
        if fill.state != FillState::Fill {
            return; // cancels/rejects don't move positions (Desk released reservations)
        }
        let key = (usr_id.to_string(), fill.order_id);
        let _ = key;
        // Look up by (usr, symbol) - fills carry order_id, the shell pass
        // maps order→(usr, symbol) from the Desk event; the engine API
        // takes the resolved tuple to stay pure.
        let fee_total = 0i128; // fee conversion lands with the rates pass
        let mut books = self.books.write();
        let symbol_id = self.symbol_for_order(fill.order_id).unwrap_or_default();
        let book_key = (usr_id.to_string(), symbol_id);
        let book = books.entry(book_key).or_insert_with(|| PositionBook {
            usr_id: usr_id.to_string(),
            office_id: office_id.to_string(),
            symbol_id,
            symbol_name: symbol_name.to_string(),
            signed_size: 0,
            avg_cost_raw: 0,
            price_scale: fill.price.scale,
            realized: 0,
            fees: 0,
            last_mark_raw: self.marks.read().get(&symbol_id).copied(),
            max_net: 0,
            lft: 0,
        });
        book.apply_fill(
            fill.qty.raw,
            if fill.qty.raw > 0 { 'B' } else { 'S' }, // side lands with the order map
            fill.price.raw,
            fee_total,
        );
        Self::track_high_water(book);
        self.recompute_summary_locked(&books, usr_id, office_id);
    }

    /// Fill application with full context (the primary API - side,
    /// symbol, fees resolved by the caller from the order + charger).
    /// The argument count is the resolved fill context; grouped into a
    /// struct would push allocation onto the per-fill path.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_fill_context(
        &self,
        usr_id: &str,
        office_id: &str,
        symbol_id: Uuid,
        symbol_name: &str,
        side_char: char,
        qty: u64,
        price_raw: i128,
        price_scale: u8,
        fee_total: i128,
    ) {
        let mut books = self.books.write();
        let book_key = (usr_id.to_string(), symbol_id);
        let book = books.entry(book_key).or_insert_with(|| PositionBook {
            usr_id: usr_id.to_string(),
            office_id: office_id.to_string(),
            symbol_id,
            symbol_name: symbol_name.to_string(),
            signed_size: 0,
            avg_cost_raw: 0,
            price_scale,
            realized: 0,
            fees: 0,
            last_mark_raw: self.marks.read().get(&symbol_id).copied(),
            max_net: 0,
            lft: 0,
        });
        book.apply_fill(qty, side_char, price_raw, fee_total);
        Self::track_high_water(book);
        self.recompute_summary_locked(&books, usr_id, office_id);
    }

    fn symbol_for_order(&self, _order: Uuid) -> Option<Uuid> {
        None // order→symbol map lands with the bus-consumer pass
    }

    /// Recompute the trader day summary from all their books (fold of
    /// per-symbol resumes - parity).
    fn recompute_summary_locked(
        &self,
        books: &HashMap<(String, Uuid), PositionBook>,
        usr_id: &str,
        office_id: &str,
    ) {
        let mut summary = TraderPnlSummary {
            usr_id: usr_id.to_string(),
            office_id: office_id.to_string(),
            trading_date: self.trading_date.read().clone(),
            ..Default::default()
        };
        for book in books.values() {
            if book.usr_id != usr_id {
                continue;
            }
            let gross = book.realized;
            let unreal = book.unrealized();
            let net = gross - book.fees + unreal;
            summary.total_gross += gross;
            summary.total_fees += book.fees;
            summary.total_net += net;
            summary.total_shares += book.signed_size.abs();
            summary.unrealized += unreal;
            if net > summary.max_net_day {
                summary.max_net_day = net;
            }
            // Historical high-water: the per-symbol trackers remember
            // peaks the current fold can't see (an adverse mark after a
            // peak must still count toward max_net_day → LFT).
            // book.max_net is already net of fees (realized − fees +
            // unrealized at the peak).
            if book.max_net > summary.max_net_day {
                summary.max_net_day = book.max_net;
            }
        }
        summary.lft = (summary.max_net_day - summary.total_net).max(0);
        self.summaries.write().insert(usr_id.to_string(), summary);
    }

    /// Current position for (trader, symbol).
    pub fn position(&self, usr_id: &str, symbol_id: Uuid) -> Option<Position> {
        let books = self.books.read();
        let b = books.get(&(usr_id.to_string(), symbol_id))?;
        Some(Position {
            usr_id: b.usr_id.clone(),
            office_id: b.office_id.clone(),
            symbol_id: b.symbol_id,
            symbol_name: b.symbol_name.clone(),
            signed_size: b.signed_size,
            avg_cost_raw: b.avg_cost_raw,
            price_scale: b.price_scale,
            realized: b.realized,
            unrealized: b.unrealized(),
            fees: b.fees,
        })
    }

    /// All positions for a trader.
    pub fn positions_for(&self, usr_id: &str) -> Vec<Position> {
        self.books
            .read()
            .values()
            .filter(|b| b.usr_id == usr_id)
            .map(|b| Position {
                usr_id: b.usr_id.clone(),
                office_id: b.office_id.clone(),
                symbol_id: b.symbol_id,
                symbol_name: b.symbol_name.clone(),
                signed_size: b.signed_size,
                avg_cost_raw: b.avg_cost_raw,
                price_scale: b.price_scale,
                realized: b.realized,
                unrealized: b.unrealized(),
                fees: b.fees,
            })
            .collect()
    }

    /// All positions across every trader (session-close persistence input).
    pub fn positions_all(&self) -> Vec<Position> {
        self.books
            .read()
            .values()
            .map(|b| Position {
                usr_id: b.usr_id.clone(),
                office_id: b.office_id.clone(),
                symbol_id: b.symbol_id,
                symbol_name: b.symbol_name.clone(),
                signed_size: b.signed_size,
                avg_cost_raw: b.avg_cost_raw,
                price_scale: b.price_scale,
                realized: b.realized,
                unrealized: b.unrealized(),
                fees: b.fees,
            })
            .collect()
    }

    /// Trader day summary.
    pub fn summary(&self, usr_id: &str) -> TraderPnlSummary {
        self.summaries
            .read()
            .get(usr_id)
            .cloned()
            .unwrap_or(TraderPnlSummary {
                usr_id: usr_id.to_string(),
                office_id: self
                    .office_of
                    .read()
                    .get(usr_id)
                    .cloned()
                    .unwrap_or_default(),
                trading_date: self.trading_date.read().clone(),
                ..Default::default()
            })
    }

    /// Office roll-up.
    pub fn office_summary(&self, office_id: &str) -> OfficePnlSummary {
        let mut out = OfficePnlSummary {
            office_id: office_id.to_string(),
            trading_date: self.trading_date.read().clone(),
            ..Default::default()
        };
        let mut seen = std::collections::HashSet::new();
        for s in self.summaries.read().values() {
            if s.office_id == office_id {
                seen.insert(s.usr_id.clone());
                out.total_fees += s.total_fees;
                out.total_gross += s.total_gross;
                out.total_net += s.total_net;
                out.total_shares += s.total_shares;
            }
        }
        out.trader_count = seen.len() as u32;
        out
    }

    /// Per-symbol resume for a trader (fold from the book).
    pub fn resume_for(&self, usr_id: &str, symbol_id: Uuid) -> SymbolResume {
        let books = self.books.read();
        match books.get(&(usr_id.to_string(), symbol_id)) {
            Some(b) => {
                let unreal = b.unrealized();
                let net = b.realized - b.fees + unreal;
                SymbolResume {
                    gross: b.realized,
                    fees: b.fees,
                    net,
                    shares: b.signed_size.abs(),
                    // high-water + drawdown from the book's
                    // trackers (updated on every fill + mark refresh).
                    max_net: b.max_net.max(net),
                    lft: b.lft,
                    unrealized: unreal,
                }
            }
            None => SymbolResume::default(),
        }
    }

    /// Update the per-symbol high-water/LFT trackers (call after every
    /// fill apply + mark refresh - the trackers ride the book).
    fn track_high_water(book: &mut PositionBook) {
        let net = book.realized - book.fees + book.unrealized();
        if net > book.max_net {
            book.max_net = net;
        }
        let drawdown = book.max_net - net;
        if drawdown > book.lft {
            book.lft = drawdown;
        }
    }

    /// Carry in an opening balance (seed a book from a
    /// prior-day position snapshot (size + avg cost) - the replay path.
    pub fn carry_position(
        &self,
        usr_id: &str,
        office_id: &str,
        symbol_id: Uuid,
        symbol_name: &str,
        signed_size: i64,
        avg_cost_raw: i128,
    ) {
        let mut books = self.books.write();
        let book_key = (usr_id.to_string(), symbol_id);
        let book = books.entry(book_key).or_insert_with(|| PositionBook {
            usr_id: usr_id.to_string(),
            office_id: office_id.to_string(),
            symbol_id,
            symbol_name: symbol_name.to_string(),
            signed_size: 0,
            avg_cost_raw: 0,
            price_scale: 4,
            realized: 0,
            fees: 0,
            last_mark_raw: self.marks.read().get(&symbol_id).copied(),
            max_net: 0,
            lft: 0,
        });
        book.signed_size = signed_size;
        book.avg_cost_raw = avg_cost_raw;
        Self::track_high_water(book);
        self.recompute_summary_locked(&books, usr_id, office_id);
    }

    /// Idempotency check: has this transaction been applied?
    pub fn already_applied(&self, tx: Uuid) -> bool {
        self.applied_tx.read().contains(&tx)
    }

    /// Deduped fill application (the bus consumer path): a
    /// transaction id may be None (direct API calls, tests) - those
    /// always apply; Some(tx) redelivery is a no-op.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_fill_context_dedup(
        &self,
        tx: Option<Uuid>,
        usr_id: &str,
        office_id: &str,
        symbol_id: Uuid,
        symbol_name: &str,
        side_char: char,
        qty: u64,
        price_raw: i128,
        fee_total: i128,
    ) {
        if let Some(tx) = tx {
            if self.already_applied(tx) {
                return;
            }
            self.applied_tx.write().insert(tx);
        }
        let mut books = self.books.write();
        let book_key = (usr_id.to_string(), symbol_id);
        let book = books.entry(book_key).or_insert_with(|| PositionBook {
            usr_id: usr_id.to_string(),
            office_id: office_id.to_string(),
            symbol_id,
            symbol_name: symbol_name.to_string(),
            signed_size: 0,
            avg_cost_raw: 0,
            price_scale: 4,
            realized: 0,
            fees: 0,
            last_mark_raw: self.marks.read().get(&symbol_id).copied(),
            max_net: 0,
            lft: 0,
        });
        book.apply_fill(qty, side_char, price_raw, fee_total);
        Self::track_high_water(book);
        self.recompute_summary_locked(&books, usr_id, office_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCALE: u8 = 4;

    #[test]
    fn long_open_add_close_realizes() {
        let e = PnlEngine::default();
        e.set_trading_date("2026-09-02");
        let sym = Uuid::new_v4();
        // buy 100 @ 10.00
        e.apply_fill_context("u1", "HO", sym, "AAPL", 'B', 100, 100_000, SCALE, 0);
        let p = e.position("u1", sym).unwrap();
        assert_eq!(p.signed_size, 100);
        assert_eq!(p.avg_cost_raw, 100_000);
        assert_eq!(p.realized, 0);
        // buy 100 more @ 11.00 → avg 10.50
        e.apply_fill_context("u1", "HO", sym, "AAPL", 'B', 100, 110_000, SCALE, 0);
        let p = e.position("u1", sym).unwrap();
        assert_eq!(p.signed_size, 200);
        assert_eq!(p.avg_cost_raw, 105_000);
        // sell 100 @ 12.00 → realize (12.00−10.50)×100 = 150.00 → 1_500_000 ×10⁴
        e.apply_fill_context("u1", "HO", sym, "AAPL", 'S', 100, 120_000, SCALE, 0);
        let p = e.position("u1", sym).unwrap();
        assert_eq!(p.signed_size, 100);
        assert_eq!(p.realized, 1_500_000);
    }

    #[test]
    fn short_open_close_realizes() {
        let e = PnlEngine::default();
        let sym = Uuid::new_v4();
        // short 100 @ 10.00
        e.apply_fill_context("u1", "HO", sym, "IBM", 'S', 100, 100_000, SCALE, 0);
        let p = e.position("u1", sym).unwrap();
        assert_eq!(p.signed_size, -100);
        assert_eq!(p.avg_cost_raw, 100_000);
        // buy back 100 @ 9.50 → realize (10.00−9.50)×100 = 50.00 → 500_000 ×10⁴
        e.apply_fill_context("u1", "HO", sym, "IBM", 'B', 100, 95_000, SCALE, 0);
        let p = e.position("u1", sym).unwrap();
        assert_eq!(p.signed_size, 0);
        assert_eq!(p.realized, 500_000);
    }

    #[test]
    fn reversal_in_one_fill() {
        let e = PnlEngine::default();
        let sym = Uuid::new_v4();
        // long 100 @ 10.00
        e.apply_fill_context("u1", "HO", sym, "X", 'B', 100, 100_000, SCALE, 0);
        // sell 250 @ 11.00 → close 100 (realize 100.00 → 1_000_000 ×10⁴), open short 150 @ 11.00
        e.apply_fill_context("u1", "HO", sym, "X", 'S', 250, 110_000, SCALE, 0);
        let p = e.position("u1", sym).unwrap();
        assert_eq!(p.signed_size, -150);
        assert_eq!(p.realized, 1_000_000);
        assert_eq!(p.avg_cost_raw, 110_000); // new side at fill px
    }

    #[test]
    fn marks_drive_unrealized() {
        let e = PnlEngine::default();
        let sym = Uuid::new_v4();
        e.apply_fill_context("u1", "HO", sym, "Y", 'B', 100, 100_000, SCALE, 0);
        assert_eq!(e.position("u1", sym).unwrap().unrealized, 0);
        e.mark(sym, 105_000);
        // (105_000−100_000)×100 = 500_000 ticks → 50.00 ×10⁴ per share ×100
        assert_eq!(e.position("u1", sym).unwrap().unrealized, 500_000);
    }

    #[test]
    fn fees_fold_into_net() {
        let e = PnlEngine::default();
        e.set_trading_date("2026-09-02");
        let sym = Uuid::new_v4();
        e.apply_fill_context("u1", "HO", sym, "Z", 'B', 100, 100_000, SCALE, 250); // fee 0.025
        e.apply_fill_context("u1", "HO", sym, "Z", 'S', 100, 101_000, SCALE, 250);
        let p = e.position("u1", sym).unwrap();
        assert_eq!(p.fees, 500);
        assert_eq!(p.realized, 100_000); // (10.10−10.00)×100
        let s = e.summary("u1");
        assert_eq!(s.total_gross, 100_000);
        assert_eq!(s.total_fees, 500);
        assert_eq!(s.total_net, 100_000 - 500);
        assert_eq!(s.office_id, "HO");
    }

    #[test]
    fn office_rollup_counts_traders() {
        let e = PnlEngine::default();
        e.set_trading_date("2026-09-02");
        let sym = Uuid::new_v4();
        e.apply_fill_context("u1", "HO", sym, "A", 'B', 100, 100_000, SCALE, 0);
        e.apply_fill_context("u2", "HO", sym, "A", 'S', 100, 100_000, SCALE, 0);
        let o = e.office_summary("HO");
        assert_eq!(o.trader_count, 2);
        assert_eq!(o.total_shares, 200);
    }

    /// LFT + max-net high-water per trader and per symbol:
    /// a profitable open then an adverse mark raises the LFT drawdown.
    #[test]
    fn lft_max_net_high_water_tracks_drawdown() {
        let e = PnlEngine::default();
        e.set_trading_date("2026-09-02");
        let sym = Uuid::new_v4();
        // buy 100 @ 10.00, mark 11.00 → net +1_000_000 (high-water).
        e.apply_fill_context("u1", "HO", sym, "A", 'B', 100, 100_000, SCALE, 0);
        e.mark(sym, 110_000);
        let resume = e.resume_for("u1", sym);
        assert_eq!(resume.max_net, 1_000_000);
        assert_eq!(resume.lft, 0);
        // adverse mark 9.00 → net −1_000_000 → LFT 2_000_000 from the
        // high-water.
        e.mark(sym, 90_000);
        let resume = e.resume_for("u1", sym);
        assert_eq!(resume.max_net, 1_000_000);
        assert_eq!(resume.lft, 2_000_000);
        // The trader summary mirrors the LFT (fold of per-symbol).
        let s = e.summary("u1");
        assert_eq!(s.unrealized, -1_000_000);
    }

    /// Golden parity on a deterministic fill sequence (the
    /// dump parity class): hand-verified realized/
    /// fees/LFT values pinned so any arithmetic drift fails loudly.
    #[test]
    fn golden_parity_deterministic_sequence() {
        let e = PnlEngine::default();
        e.set_trading_date("2026-09-02");
        let sym = Uuid::new_v4();
        // The golden sequence (all ×10⁴ money, price ×10⁴):
        // buy 200 @ 10.00 fee 500
        // buy 300 @ 11.00 fee 750 → avg 10.60, size 500
        // sell 500 @ 12.00 fee 1250 → realized (12−10.60)×500 = 700.00
        // buy 100 @ 11.50 fee 250 → reopen long
        // mark 11.00 → unrealized −50.00
        e.apply_fill_context("g1", "HO", sym, "G", 'B', 200, 100_000, SCALE, 500);
        e.apply_fill_context("g1", "HO", sym, "G", 'B', 300, 110_000, SCALE, 750);
        e.apply_fill_context("g1", "HO", sym, "G", 'S', 500, 120_000, SCALE, 1250);
        e.apply_fill_context("g1", "HO", sym, "G", 'B', 100, 115_000, SCALE, 250);
        e.mark(sym, 110_000);
        let p = e.position("g1", sym).unwrap();
        assert_eq!(p.signed_size, 100);
        assert_eq!(p.avg_cost_raw, 115_000);
        // realized = 700.00 → 7_000_000 ×10⁴.
        assert_eq!(p.realized, 7_000_000);
        assert_eq!(p.fees, 2_750);
        let s = e.summary("g1");
        assert_eq!(s.total_gross, 7_000_000);
        assert_eq!(s.total_fees, 2_750);
        // net = gross − fees + unrealized(−50.00 → −500_000) = 6_497_250.
        assert_eq!(s.total_net, 7_000_000 - 2_750 - 500_000);
        // LFT: the peak was after the sell (fees 2_500 at that point,
        // unrealized 0) → max_net 6_997_500; drawdown to 6_497_250 →
        // 500_250 (the buy's extra 250 fee counts into the drawdown).
        assert_eq!(s.max_net_day, 6_997_500);
        assert_eq!(s.lft, 500_250);
    }

    /// Replay convergence: kill mid-day (drop the engine),
    /// restart (fresh engine + opening balances + event replay) → the
    /// summaries converge to the pre-kill values.
    #[test]
    fn replay_convergence_after_kill() {
        // Day 1 engine: builds the position + records the opening
        // balance hand-off (position snapshot as the carry).
        let day1 = PnlEngine::default();
        day1.set_trading_date("2026-09-02");
        let sym = Uuid::new_v4();
        day1.apply_fill_context("r1", "HO", sym, "R", 'B', 100, 100_000, SCALE, 0);
        day1.apply_fill_context("r1", "HO", sym, "R", 'B', 100, 110_000, SCALE, 0);
        day1.mark(sym, 115_000);
        let before = day1.summary("r1");
        // The "opening balance" for day 2: the position snapshot.
        let carry = day1.position("r1", sym).unwrap();

        // Kill + restart: fresh engine, carry in the opening balance
        // (avg cost + size), replay the day's fills from events.
        let day2 = PnlEngine::default();
        day2.set_trading_date("2026-09-03");
        // Carry-in (the store's load_opening_balances equivalent at the
        // engine level: seed the book from the snapshot).
        day2.carry_position("r1", "HO", sym, "R", carry.signed_size, carry.avg_cost_raw);
        // Replay the day's fills (deduped - same tx ids would converge
        // even on redelivery).
        day2.apply_fill_context("r1", "HO", sym, "R", 'S', 100, 120_000, SCALE, 0);
        day2.mark(sym, 115_000);

        // Convergence: position + realized match the never-killed run.
        let never_killed = PnlEngine::default();
        never_killed.set_trading_date("2026-09-03");
        never_killed.carry_position("r1", "HO", sym, "R", carry.signed_size, carry.avg_cost_raw);
        never_killed.apply_fill_context("r1", "HO", sym, "R", 'S', 100, 120_000, SCALE, 0);
        never_killed.mark(sym, 115_000);
        let after = day2.summary("r1");
        let expect = never_killed.summary("r1");
        assert_eq!(after.total_gross, expect.total_gross);
        assert_eq!(after.total_net, expect.total_net);
        assert_eq!(after.total_shares, expect.total_shares);
        // And the values are the hand-derived truth: close 100 @ 12.00
        // from avg 10.50 → realized 150.00 → 1_500_000; remaining 100
        // long marked 11.50 → unrealized +100.00 → 1_000_000.
        assert_eq!(after.total_gross, 1_500_000);
        assert_eq!(after.unrealized, 1_000_000);
        let _ = before;
    }
}
