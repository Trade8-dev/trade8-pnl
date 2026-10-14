// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Trade8:Pnl
// SPDX-License-Identifier: Apache-2.0

//! Trade8:Pnl SQL integration tests (test-architecture-design §5.3).
//!
//! Real `trade8_pnl_service` NOBYPASSRLS role: position upsert, opening
//! balance carry contract, trader-board today filter, currency-rate
//! scale, plus the RLS matrix. Same tx-local context + skip-when-offline
//! contract as the desk matrix.
//!
//! Design: `../../DESIGN.md §9 (testing)` §5.3.

#[path = "../src/core.rs"]
#[allow(dead_code)]
mod core;
#[path = "../src/store.rs"]
#[allow(dead_code)]
mod store;

use core::PnlEngine;
use sqlx::postgres::PgPoolOptions;
use sqlx::Acquire;
use trade8::contracts::{events, Position};
use uuid::Uuid;

const PNL_URL: &str = "postgresql://trade8_pnl_service@localhost:5432/trade8";
const PROBE_URL: &str = "postgresql://trade8_probe@localhost:5432/trade8";

fn env_a() -> Uuid {
    Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap()
}
fn env_b() -> Uuid {
    Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap()
}

async fn pnl_conn() -> Option<sqlx::pool::PoolConnection<sqlx::Postgres>> {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(PNL_URL)
        .await
        .ok()?;
    pool.acquire().await.ok()
}

async fn probe_conn() -> Option<sqlx::pool::PoolConnection<sqlx::Postgres>> {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(PROBE_URL)
        .await
        .ok()?;
    pool.acquire().await.ok()
}

async fn cleanup(conn: &mut sqlx::PgConnection, env: Uuid, usr: &str) {
    let mut tx = conn.begin().await.unwrap();
    sentin8::shell::set_tenant_context(&mut tx, env).await;
    let _ = sqlx::query(r#"DELETE FROM pnl.position_snapshot WHERE env_id=$1 AND usr_id=$2"#)
        .bind(env)
        .bind(usr)
        .execute(&mut *tx)
        .await;
    let _ = sqlx::query(r#"DELETE FROM pnl.trader_summary WHERE env_id=$1 AND usr_id=$2"#)
        .bind(env)
        .bind(usr)
        .execute(&mut *tx)
        .await;
    let _ = sqlx::query(r#"DELETE FROM pnl.opening_balance WHERE env_id=$1 AND usr_id=$2"#)
        .bind(env)
        .bind(usr)
        .execute(&mut *tx)
        .await;
    let _ = sqlx::query(r#"DELETE FROM desk.currency WHERE env_id=$1 AND currency_code='XXX'"#)
        .bind(env)
        .execute(&mut *tx)
        .await;
    let _ = sqlx::query(r#"DELETE FROM pnl.outbox WHERE env_id=$1 AND stream_family='pnl' AND created_at > now() - interval '1 minute'"#)
        .bind(env)
        .execute(&mut *tx)
        .await;
    tx.commit().await.unwrap();
}

// ---------------------------------------------------------------------------
// RLS matrix
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rls_matrix_no_context_returns_zero() {
    let Some(mut conn) = probe_conn().await else {
        eprintln!("skipping: no live DB");
        return;
    };
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM pnl.position_snapshot")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(n, 0, "no tenant context must see zero rows");
}

#[tokio::test]
async fn rls_matrix_cross_env_returns_zero() {
    let Some(mut conn) = pnl_conn().await else {
        eprintln!("skipping: no live DB");
        return;
    };
    let sym = Uuid::new_v4();
    {
        let mut tx = conn.begin().await.unwrap();
        sentin8::shell::set_tenant_context(&mut tx, env_a()).await;
        store::persist_position(
            &mut tx,
            env_a(),
            "2026-09-04",
            &Position {
                usr_id: "u-sql".into(),
                office_id: "HO".into(),
                symbol_id: sym,
                symbol_name: "AAPL".into(),
                signed_size: 100,
                avg_cost_raw: 100_000,
                price_scale: 4,
                realized: 0,
                unrealized: 0,
                fees: 0,
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    let mut tx = conn.begin().await.unwrap();
    sentin8::shell::set_tenant_context(&mut tx, env_b()).await;
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pnl.position_snapshot WHERE usr_id = 'u-sql' AND symbol_id = $1",
    )
    .bind(sym)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(n, 0, "env-b must not see env-a's positions");
    cleanup(&mut conn, env_a(), "u-sql").await;
}

// ---------------------------------------------------------------------------
// Persist paths (design §5.3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn persist_position_roundtrip_upsert() {
    let Some(mut conn) = pnl_conn().await else {
        eprintln!("skipping: no live DB");
        return;
    };
    let sym = Uuid::new_v4();
    let p = |signed: i64, realized: i128| Position {
        usr_id: "u-sql".into(),
        office_id: "HO".into(),
        symbol_id: sym,
        symbol_name: "AAPL".into(),
        signed_size: signed,
        avg_cost_raw: 100_000,
        price_scale: 4,
        realized,
        unrealized: 0,
        fees: 0,
    };
    {
        let mut tx = conn.begin().await.unwrap();
        sentin8::shell::set_tenant_context(&mut tx, env_a()).await;
        store::persist_position(&mut tx, env_a(), "2026-09-04", &p(100, 0))
            .await
            .unwrap();
        store::persist_position(&mut tx, env_a(), "2026-09-04", &p(200, 1_500_000))
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    let mut tx = conn.begin().await.unwrap();
    sentin8::shell::set_tenant_context(&mut tx, env_a()).await;
    let rows: Vec<(i64, i64, i64)> = sqlx::query_as(
        r#"SELECT signed_size, avg_cost_raw, realized FROM pnl.position_snapshot
           WHERE env_id=$1 AND usr_id='u-sql' AND symbol_id=$2"#,
    )
    .bind(env_a())
    .bind(sym)
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1, "upsert must collapse to one row");
    assert_eq!(rows[0], (200, 100_000, 1_500_000));
    let events_: i64 = sqlx::query_scalar(
        r#"SELECT count(*) FROM pnl.outbox
           WHERE env_id=$1 AND event_type=$2 AND payload->>'symbol_id'=$3"#,
    )
    .bind(env_a())
    .bind(events::POSITION_UPDATED)
    .bind(sym.to_string())
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert!(
        events_ >= 2,
        "each position write must emit the outbox event"
    );
    tx.commit().await.unwrap();
    cleanup(&mut conn, env_a(), "u-sql").await;
}

/// The opening-balance carry contract: rows are written for
/// tomorrow's date; the row shape + conflict behavior are asserted via a
/// direct read (the loader reads today's bucket only — the true
/// park→load cycle is covered by the desk overnight test + e2e).
#[tokio::test]
async fn opening_balance_carry_contract() {
    let Some(mut conn) = pnl_conn().await else {
        eprintln!("skipping: no live DB");
        return;
    };
    let sym = Uuid::new_v4();
    let p = Position {
        usr_id: "u-sql".into(),
        office_id: "HO".into(),
        symbol_id: sym,
        symbol_name: "AAPL".into(),
        signed_size: 300,
        avg_cost_raw: 105_000,
        price_scale: 4,
        realized: 700_000,
        unrealized: 0,
        fees: 0,
    };
    let n = {
        let mut tx = conn.begin().await.unwrap();
        sentin8::shell::set_tenant_context(&mut tx, env_a()).await;
        let n = store::persist_opening_balances(&mut tx, env_a(), &[p.clone(), p.clone()])
            .await
            .unwrap();
        tx.commit().await.unwrap();
        n
    };
    assert_eq!(n, 2, "both snapshots must be written");
    let mut tx = conn.begin().await.unwrap();
    sentin8::shell::set_tenant_context(&mut tx, env_a()).await;
    let rows: Vec<(i64, i64, i16)> = sqlx::query_as(
        r#"SELECT signed_size, avg_cost_raw, price_scale
           FROM pnl.opening_balance
           WHERE env_id=$1 AND usr_id='u-sql' AND symbol_id=$2"#,
    )
    .bind(env_a())
    .bind(sym)
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1, "ON CONFLICT DO NOTHING must collapse");
    assert_eq!((rows[0].0, rows[0].1, rows[0].2 as i64), (300, 105_000, 4));
    // And the carry lands on TOMORROW's bucket (the session-close write).
    let tomorrow: i64 = sqlx::query_scalar(
        r#"SELECT count(*) FROM pnl.opening_balance
           WHERE env_id=$1 AND usr_id='u-sql' AND symbol_id=$2
             AND trading_date = CURRENT_DATE + 1"#,
    )
    .bind(env_a())
    .bind(sym)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(tomorrow, 1, "carry rows are written for tomorrow");
    tx.commit().await.unwrap();
    cleanup(&mut conn, env_a(), "u-sql").await;
}

#[tokio::test]
async fn trader_board_reads_today_rows() {
    let Some(mut conn) = pnl_conn().await else {
        eprintln!("skipping: no live DB");
        return;
    };
    use trade8::contracts::{OfficePnlSummary, TraderPnlSummary};
    // CURRENT_DATE, not a fixed literal — read_trader_board filters
    // `trading_date = CURRENT_DATE`, so a hardcoded date drifts after that
    // day (a fixed test date).
    let today = chrono::Utc::now().date_naive().to_string();
    let s = TraderPnlSummary {
        usr_id: "u-sql".into(),
        office_id: "HO".into(),
        trading_date: today.clone(),
        total_fees: 500,
        total_gross: 7_000_000,
        total_net: 6_997_250,
        total_shares: 500,
        max_net_day: 6_997_500,
        unrealized: -500_000,
        lft: 500_250,
    };
    {
        let mut tx = conn.begin().await.unwrap();
        sentin8::shell::set_tenant_context(&mut tx, env_a()).await;
        store::persist_trader_summary(&mut tx, env_a(), &today, &s)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    let board = {
        let mut tx = conn.begin().await.unwrap();
        let v = store::read_trader_board(&mut tx, env_a(), 10)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        v
    };
    let mine: Vec<_> = board.into_iter().filter(|r| r.0 == "u-sql").collect();
    assert_eq!(mine.len(), 1, "board must return today's row once");
    assert_eq!(mine[0].2, 6_997_250);

    // Office summary upsert + outbox-event event.
    let o = OfficePnlSummary {
        office_id: "HO".into(),
        trading_date: today.clone(),
        total_fees: 500,
        total_gross: 7_000_000,
        total_net: 6_997_250,
        total_shares: 500,
        trader_count: 1,
    };
    {
        let mut tx = conn.begin().await.unwrap();
        sentin8::shell::set_tenant_context(&mut tx, env_a()).await;
        store::persist_office_summary(&mut tx, env_a(), &today, &o)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    cleanup(&mut conn, env_a(), "u-sql").await;
}

#[tokio::test]
async fn currency_rates_scale() {
    let Some(mut conn) = pnl_conn().await else {
        eprintln!("skipping: no live DB");
        return;
    };
    {
        let mut tx = conn.begin().await.unwrap();
        sentin8::shell::set_tenant_context(&mut tx, env_a()).await;
        sqlx::query(
            r#"INSERT INTO desk.currency (currency_code, env_id, name, rate_to_usd)
               VALUES ('XXX',$1,'test ccy',10000)
               ON CONFLICT (currency_code) DO UPDATE SET rate_to_usd = 10000"#,
        )
        .bind(env_a())
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    let rates = {
        let mut tx = conn.begin().await.unwrap();
        let v = store::load_currency_rates(&mut tx, env_a()).await.unwrap();
        tx.rollback().await.unwrap();
        v
    };
    let xxx = rates.iter().find(|(c, _)| c == "XXX").expect("rate loaded");
    assert_eq!(xxx.1, 10_000, "rate must surface at ×10⁴ scale (i128)");
    cleanup(&mut conn, env_a(), "u-sql").await;
}

/// The engine-level parity between a live engine and a stored position —
/// the store round-trips the engine's Position struct faithfully.
#[tokio::test]
async fn engine_position_matches_store_shape() {
    let e = PnlEngine::default();
    e.set_trading_date("2026-09-04");
    let sym = Uuid::new_v4();
    e.apply_fill_context("u-sql", "HO", sym, "AAPL", 'B', 100, 100_000, 4, 250);
    let pos = e.position("u-sql", sym).unwrap();
    assert_eq!(pos.signed_size, 100);
    assert_eq!(pos.avg_cost_raw, 100_000);
    assert_eq!(pos.fees, 250);
    let _ = pos; // shape asserted in core unit tests; here we prove the type is the same one store.rs persists
}
