// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Trade8:Pnl
// SPDX-License-Identifier: Apache-2.0

//! Pnl bus consumers (pnl design §7).
//!
//! Three consumer groups over `trade8::client::run_consumer`:
//! - **router** family: `fill:executed/cancelled/rejected` → position
//!   engine + fee accumulation + persist + summary
//!   recompute.
//! - **desk** family: `session:state` (close → session-close persist
//!   pass) + `order:overnight:restored` (overnight
//!   verification).
//! - **feed** family: `trade:printed` → marks refresh unrealized.
//!
//! All handlers idempotent: fills are keyed by transaction_id (the
//! position engine dedupes), redelivery converges.

use sqlx::Acquire;
use std::sync::Arc;

use trade8::client::{BusMessage, HandleOutcome};
use trade8::contracts::events;
use uuid::Uuid;

use crate::core::PnlEngine;

/// Spawn all three consumers.
pub fn spawn_consumers(
    pool: sqlx::PgPool,
    bus_url: &str,
    auth_issuer: &str,
    engine: Arc<PnlEngine>,
) {
    for family in ["router", "desk", "feed"] {
        let pool = pool.clone();
        let bus_url = bus_url.to_string();
        let auth_issuer = auth_issuer.to_string();
        let engine = engine.clone();
        let group = format!("trade8:pnl:{family}");
        tokio::spawn(async move {
            // Cert-presenting Bus client (CERTFREE-4V): the per-family
            // workload CERTIFICATE is the identity — no DCR, no token;
            // the mint retries while the fleet boots.
            let bus = loop {
                match sentin8::bus::client::BusClient::for_workload(
                    &bus_url,
                    &auth_issuer,
                    &group,
                )
                .await
                {
                    Ok(c) => break c,
                    Err(e) => {
                        tracing::debug!(error = %e, "pnl bus client: certificate not ready yet");
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                }
            };
            let bus = Arc::new(bus);
            trade8::client::run_consumer(bus, family, &group, move |msg: BusMessage| {
                let engine = engine.clone();
                let pool = pool.clone();
                async move { handle_family(family, msg, &engine, &pool).await }
            })
            .await;
        });
    }
}

async fn handle_family(
    family: &str,
    msg: BusMessage,
    engine: &Arc<PnlEngine>,
    pool: &sqlx::PgPool,
) -> HandleOutcome {
    match family {
        "router" => handle_fill(msg, engine, pool).await,
        "desk" => handle_desk(msg, engine, pool).await,
        "feed" => handle_mark(msg, engine).await,
        _ => HandleOutcome::Ack,
    }
}

/// Fills: apply to the position book, persist the
/// position + resume, recompute + persist the trader/office summaries.
async fn handle_fill(
    msg: BusMessage,
    engine: &Arc<PnlEngine>,
    pool: &sqlx::PgPool,
) -> HandleOutcome {
    if msg.event_type != events::FILL_EXECUTED {
        return HandleOutcome::Ack;
    }
    let p = &msg.payload;
    let Ok(env_id) = Uuid::parse_str(&msg.env_id) else {
        return HandleOutcome::Ack;
    };
    let usr_id = p["usr_id"].as_str().unwrap_or("unknown").to_string();
    let office = "HO".to_string(); // profile-resolved at summary time
    let symbol_id = p["symbol_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok());
    let Some(symbol_id) = symbol_id else {
        return HandleOutcome::Ack;
    };
    let side = p["side"]
        .as_str()
        .and_then(|s| s.chars().next())
        .unwrap_or('B');
    let qty = p["qty"].as_u64().unwrap_or(0);
    let price_raw = p["price_raw"].as_i64().unwrap_or(0) as i128;
    // Fee accumulation: each fee converts at its currency's
    // rate_to_usd (×10⁴, from trade8.currency) then sums — the engine
    // applies the USD-scaled total to realized.
    let rates: Vec<(String, i128)> = [
        "sec_fee",
        "activity_fee",
        "gateway_fee",
        "execution_fee",
        "clearing_fee",
    ]
    .iter()
    .filter_map(|k| {
        let m = &p["fees"][k];
        let amount = m["amount"].as_i64()? as i128;
        let ccy = m["currency"].as_str().unwrap_or("USD").to_string();
        Some((ccy, amount))
    })
    .collect();
    let fee_total: i128 = match pool.acquire().await {
        Ok(mut conn) => {
            sentin8::shell::set_tenant_context(&mut conn, env_id).await;
            // RLS context is transaction-local (DESIGN.md §5) —
            // begin() pins the tenant context to the SAME tx as the DML/reads.
            let mut tx = match conn.begin().await {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(error = %e, "pnl: tenant tx begin failed");
                    return HandleOutcome::Nack;
                }
            };
            let table = crate::store::load_currency_rates(&mut tx, env_id)
                .await
                .unwrap_or_default();
            tx.commit().await.ok(); // dropping an uncommitted tx rolls back
            rates
                .iter()
                .map(|(ccy, amount)| {
                    let rate = table
                        .iter()
                        .find(|(c, _)| c == ccy)
                        .map(|(_, r)| *r)
                        .unwrap_or(10_000); // 1.0 when unknown
                    amount * rate / 10_000
                })
                .sum()
        }
        Err(_) => rates.iter().map(|(_, a)| *a).sum(), // no DB — raw sum
    };
    // Idempotency: the engine's seen-set on transaction_id.
    let tx_id = p["transaction_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok());
    if let Some(tx) = tx_id {
        if engine.already_applied(tx) {
            return HandleOutcome::Ack;
        }
    }
    engine.apply_fill_context_dedup(
        tx_id,
        &usr_id,
        &office,
        symbol_id,
        p["symbol_name"].as_str().unwrap_or(""),
        side,
        qty,
        price_raw,
        fee_total,
    );
    // Persist position + summaries (fail → nack; redelivery converges
    // because the engine dedupe makes re-apply a no-op).
    let Ok(mut conn) = pool.acquire().await else {
        return HandleOutcome::Nack;
    };
    sentin8::shell::set_tenant_context(&mut conn, env_id).await;
    // RLS context is transaction-local (DESIGN.md §5) —
    // begin() pins the tenant context to the SAME tx as the DML/reads.
    let mut tx = match conn.begin().await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "pnl: tenant tx begin failed");
            return HandleOutcome::Nack;
        }
    };
    let trading_date = chrono::Utc::now().date_naive().to_string();
    if let Some(pos) = engine.position(&usr_id, symbol_id) {
        if crate::store::persist_position(&mut tx, env_id, &trading_date, &pos)
            .await
            .is_err()
        {
            return HandleOutcome::Nack;
        }
    }
    let summary = engine.summary(&usr_id);
    if crate::store::persist_trader_summary(&mut tx, env_id, &trading_date, &summary)
        .await
        .is_err()
    {
        return HandleOutcome::Nack;
    }
    let office_summary = engine.office_summary(&summary.office_id);
    if crate::store::persist_office_summary(&mut tx, env_id, &trading_date, &office_summary)
        .await
        .is_err()
    {
        return HandleOutcome::Nack;
    }
    HandleOutcome::Ack
}

/// Desk session state (close persist) + overnight verification
/// (parked-vs-restored reconcile → mismatch events).
async fn handle_desk(
    msg: BusMessage,
    engine: &Arc<PnlEngine>,
    pool: &sqlx::PgPool,
) -> HandleOutcome {
    let Ok(env_id) = Uuid::parse_str(&msg.env_id) else {
        return HandleOutcome::Ack;
    };
    match msg.event_type.as_str() {
        events::SESSION_STATE => {
            let state = msg.payload["state"].as_str().unwrap_or("");
            if state == "closed" {
                // Session-close persist pass: positions +
                // resumes + summaries + next-day opening balances.
                let Ok(mut conn) = pool.acquire().await else {
                    return HandleOutcome::Nack;
                };
                sentin8::shell::set_tenant_context(&mut conn, env_id).await;
                // RLS context is transaction-local (DESIGN.md §5) —
                // begin() pins the tenant context to the SAME tx as the DML/reads.
                let mut tx = match conn.begin().await {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(error = %e, "pnl: tenant tx begin failed");
                        return HandleOutcome::Nack;
                    }
                };
                let trading_date = chrono::Utc::now().date_naive().to_string();
                let mut traders: Vec<String> = engine
                    .positions_all()
                    .into_iter()
                    .map(|p| p.usr_id)
                    .collect();
                traders.sort();
                traders.dedup();
                for usr in &traders {
                    for pos in engine.positions_for(usr) {
                        let _ =
                            crate::store::persist_position(&mut tx, env_id, &trading_date, &pos)
                                .await;
                        let resume = engine.resume_for(usr, pos.symbol_id);
                        let _ = crate::store::persist_resume(
                            &mut tx,
                            env_id,
                            &trading_date,
                            usr,
                            pos.symbol_id,
                            &resume,
                        )
                        .await;
                    }
                    let summary = engine.summary(usr);
                    let _ = crate::store::persist_trader_summary(
                        &mut tx,
                        env_id,
                        &trading_date,
                        &summary,
                    )
                    .await;
                    let office = engine.office_summary(&summary.office_id);
                    let _ = crate::store::persist_office_summary(
                        &mut tx,
                        env_id,
                        &trading_date,
                        &office,
                    )
                    .await;
                }
                let all = engine.positions_all();
                let _ = crate::store::persist_opening_balances(&mut tx, env_id, &all).await;
                tracing::info!(traders = traders.len(), "pnl: session close persisted");
                tx.commit().await.ok(); // dropping an uncommitted tx rolls back
            }
            HandleOutcome::Ack
        }
        events::ORDER_OVERNIGHT_RESTORED => {
            // Overnight verification: the Desk restored a parked
            // order; reconcile the position book's carry for that
            // (usr, symbol) against the parked snapshot's pnl_carry — a
            // divergence persists an `overnight:mismatch` event for the
            // ops queue.
            let usr = msg.payload["usr_id"].as_str().unwrap_or("").to_string();
            let symbol_id = msg.payload["symbol_id"]
                .as_str()
                .and_then(|s| Uuid::parse_str(s).ok());
            let expected = msg.payload["pnl_carry"].as_i64().unwrap_or(0);
            if let (Some(sym), false) = (symbol_id, usr.is_empty()) {
                let actual = engine
                    .position(&usr, sym)
                    .map(|p| p.realized as i64)
                    .unwrap_or(0);
                if actual != expected {
                    let Ok(mut conn) = pool.acquire().await else {
                        return HandleOutcome::Nack;
                    };
                    sentin8::shell::set_tenant_context(&mut conn, env_id).await;
                    // RLS context is transaction-local (DESIGN.md §5) —
                    // begin() pins the tenant context to the SAME tx as the DML/reads.
                    let mut tx = match conn.begin().await {
                        Ok(t) => t,
                        Err(e) => {
                            tracing::warn!(error = %e, "pnl: tenant tx begin failed");
                            return HandleOutcome::Nack;
                        }
                    };
                    let _ = crate::store::persist_overnight_mismatch(
                        &mut tx, env_id, &usr, sym, expected, actual,
                    )
                    .await;
                    tx.commit().await.ok(); // dropping an uncommitted tx rolls back
                }
            }
            HandleOutcome::Ack
        }
        _ => HandleOutcome::Ack,
    }
}

/// Marks: prints refresh the unrealized marks.
async fn handle_mark(msg: BusMessage, engine: &Arc<PnlEngine>) -> HandleOutcome {
    if msg.event_type != events::TRADE_PRINTED {
        return HandleOutcome::Ack;
    }
    let p = &msg.payload;
    let Some(symbol_id) = p["symbol_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
    else {
        return HandleOutcome::Ack;
    };
    let Some(raw) = p["price_raw"].as_i64() else {
        return HandleOutcome::Ack;
    };
    engine.mark(symbol_id, raw as i128);
    HandleOutcome::Ack
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill_msg(tx: Uuid, usr: &str, side: char, qty: u64, price: i64, fee: i64) -> BusMessage {
        BusMessage {
            message_id: "1-1".into(),
            event_type: events::FILL_EXECUTED.into(),
            env_id: "00000000-0000-0000-0000-000000000001".into(),
            payload: serde_json::json!({
                "transaction_id": tx.to_string(),
                "order_id": Uuid::new_v4().to_string(),
                "usr_id": usr,
                "symbol_id": Uuid::new_v4().to_string(),
                "symbol_name": "AAPL",
                "side": side.to_string(),
                "qty": qty,
                "price_raw": price,
                "price_scale": 4,
                "fees": {"sec_fee": {"amount": fee, "currency": "USD"}},
            }),
            correlation_id: String::new(),
            idempotency_key: Some("k".into()),
        }
    }

    /// Replay convergence (engine level): the same fill
    /// sequence applied twice converges — dedupe makes redelivery a
    /// no-op (golden parity precondition).
    #[tokio::test]
    async fn fill_dedupe_converges() {
        let engine = Arc::new(PnlEngine::default());
        engine.set_trading_date("2026-09-03");
        let tx = Uuid::new_v4();
        let msg = fill_msg(tx, "t1", 'B', 100, 100_000, 231);
        // First apply (persist fails → Nack is fine; use a scratch pool —
        // here we assert the engine side only via handle_mark-shaped
        // direct calls since no DB is available in unit tests).
        let p = &msg.payload;
        let symbol_id = p["symbol_id"]
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok())
            .unwrap();
        engine.apply_fill_context_dedup(
            Some(tx),
            "t1",
            "HO",
            symbol_id,
            "AAPL",
            'B',
            100,
            100_000,
            231,
        );
        let pos1 = engine.position("t1", symbol_id).unwrap();
        // Redelivery: no double-apply.
        engine.apply_fill_context_dedup(
            Some(tx),
            "t1",
            "HO",
            symbol_id,
            "AAPL",
            'B',
            100,
            100_000,
            231,
        );
        let pos2 = engine.position("t1", symbol_id).unwrap();
        assert_eq!(pos1.signed_size, pos2.signed_size);
        assert_eq!(pos1.realized, pos2.realized);
        assert_eq!(pos1.fees, pos2.fees);
    }
}
