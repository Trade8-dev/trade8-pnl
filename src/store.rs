// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Trade8:Pnl
// SPDX-License-Identifier: Apache-2.0

//! Pnl DB persistence — position/resume/summary upserts,
//! opening-balance carry, and `position:updated` / `trader:summary` /
//! `office:summary` / `overnight:mismatch` outbox events.

use sqlx::PgConnection;
use trade8::contracts::{events, OfficePnlSummary, Position, SymbolResume, TraderPnlSummary};
use uuid::Uuid;

/// Upsert a position snapshot + `position:updated` outbox event.
pub async fn persist_position(
    conn: &mut PgConnection,
    env_id: Uuid,
    trading_date: &str,
    p: &Position,
) -> Result<(), sqlx::Error> {
    // context is transaction-local: set it on the executor this fn receives (DESIGN.md §5).
    sentin8::shell::set_tenant_context(conn, env_id).await;
    sqlx::query(
        r#"INSERT INTO pnl.position_snapshot
           (env_id, usr_id, office_id, symbol_id, signed_size, avg_cost_raw,
            price_scale, realized, unrealized, fees, updated_at)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10, now())
           ON CONFLICT (env_id, usr_id, symbol_id) DO UPDATE SET
             signed_size = EXCLUDED.signed_size,
             avg_cost_raw = EXCLUDED.avg_cost_raw,
             price_scale = EXCLUDED.price_scale,
             realized = EXCLUDED.realized,
             unrealized = EXCLUDED.unrealized,
             fees = EXCLUDED.fees,
             updated_at = now()"#,
    )
    .bind(env_id)
    .bind(&p.usr_id)
    .bind(&p.office_id)
    .bind(p.symbol_id)
    .bind(p.signed_size)
    .bind(p.avg_cost_raw as i64)
    .bind(p.price_scale as i16)
    .bind(p.realized as i64)
    .bind(p.unrealized as i64)
    .bind(p.fees as i64)
    .execute(&mut *conn)
    .await?;
    insert_outbox_event(
        conn,
        env_id,
        events::POSITION_UPDATED,
        serde_json::json!({
            "usr_id": p.usr_id, "office_id": p.office_id,
            "symbol_id": p.symbol_id, "signed_size": p.signed_size,
            "avg_cost_raw": p.avg_cost_raw, "trading_date": trading_date,
        }),
    )
    .await
}

/// Upsert a per-symbol resume row.
pub async fn persist_resume(
    conn: &mut PgConnection,
    env_id: Uuid,
    trading_date: &str,
    usr_id: &str,
    symbol_id: Uuid,
    r: &SymbolResume,
) -> Result<(), sqlx::Error> {
    // context is transaction-local: set it on the executor this fn receives (DESIGN.md §5).
    sentin8::shell::set_tenant_context(conn, env_id).await;
    let date = chrono::NaiveDate::parse_from_str(trading_date, "%Y-%m-%d")
        .unwrap_or_else(|_| chrono::Utc::now().date_naive());
    sqlx::query(
        r#"INSERT INTO pnl.symbol_resume
           (env_id, usr_id, symbol_id, trading_date, gross, fees, net,
            shares, max_net, lft, unrealized)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
           ON CONFLICT (env_id, usr_id, symbol_id, trading_date) DO UPDATE SET
             gross = EXCLUDED.gross, fees = EXCLUDED.fees, net = EXCLUDED.net,
             shares = EXCLUDED.shares, max_net = EXCLUDED.max_net,
             lft = EXCLUDED.lft, unrealized = EXCLUDED.unrealized"#,
    )
    .bind(env_id)
    .bind(usr_id)
    .bind(symbol_id)
    .bind(date)
    .bind(r.gross as i64)
    .bind(r.fees as i64)
    .bind(r.net as i64)
    .bind(r.shares)
    .bind(r.max_net as i64)
    .bind(r.lft as i64)
    .bind(r.unrealized as i64)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Upsert a trader day summary + `trader:summary` outbox event.
pub async fn persist_trader_summary(
    conn: &mut PgConnection,
    env_id: Uuid,
    trading_date: &str,
    s: &TraderPnlSummary,
) -> Result<(), sqlx::Error> {
    // context is transaction-local: set it on the executor this fn receives (DESIGN.md §5).
    sentin8::shell::set_tenant_context(conn, env_id).await;
    let date = chrono::NaiveDate::parse_from_str(trading_date, "%Y-%m-%d")
        .unwrap_or_else(|_| chrono::Utc::now().date_naive());
    sqlx::query(
        r#"INSERT INTO pnl.trader_summary
           (env_id, usr_id, office_id, trading_date, total_fees, total_gross,
            total_net, total_shares, max_net_day, unrealized, lft)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
           ON CONFLICT (env_id, usr_id, trading_date) DO UPDATE SET
             total_fees = EXCLUDED.total_fees,
             total_gross = EXCLUDED.total_gross,
             total_net = EXCLUDED.total_net,
             total_shares = EXCLUDED.total_shares,
             max_net_day = EXCLUDED.max_net_day,
             unrealized = EXCLUDED.unrealized,
             lft = EXCLUDED.lft"#,
    )
    .bind(env_id)
    .bind(&s.usr_id)
    .bind(&s.office_id)
    .bind(date)
    .bind(s.total_fees as i64)
    .bind(s.total_gross as i64)
    .bind(s.total_net as i64)
    .bind(s.total_shares)
    .bind(s.max_net_day as i64)
    .bind(s.unrealized as i64)
    .bind(s.lft as i64)
    .execute(&mut *conn)
    .await?;
    insert_outbox_event(
        conn,
        env_id,
        events::TRADER_SUMMARY,
        serde_json::json!({
            "usr_id": s.usr_id, "office_id": s.office_id,
            "total_net": s.total_net, "trading_date": trading_date,
        }),
    )
    .await
}

/// Upsert an office day summary + `office:summary` outbox event.
pub async fn persist_office_summary(
    conn: &mut PgConnection,
    env_id: Uuid,
    trading_date: &str,
    s: &OfficePnlSummary,
) -> Result<(), sqlx::Error> {
    // context is transaction-local: set it on the executor this fn receives (DESIGN.md §5).
    sentin8::shell::set_tenant_context(conn, env_id).await;
    let date = chrono::NaiveDate::parse_from_str(trading_date, "%Y-%m-%d")
        .unwrap_or_else(|_| chrono::Utc::now().date_naive());
    sqlx::query(
        r#"INSERT INTO pnl.office_summary
           (env_id, office_id, trading_date, total_fees, total_gross,
            total_net, total_shares, trader_count)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
           ON CONFLICT (env_id, office_id, trading_date) DO UPDATE SET
             total_fees = EXCLUDED.total_fees,
             total_gross = EXCLUDED.total_gross,
             total_net = EXCLUDED.total_net,
             total_shares = EXCLUDED.total_shares,
             trader_count = EXCLUDED.trader_count"#,
    )
    .bind(env_id)
    .bind(&s.office_id)
    .bind(date)
    .bind(s.total_fees as i64)
    .bind(s.total_gross as i64)
    .bind(s.total_net as i64)
    .bind(s.total_shares)
    .bind(s.trader_count as i32)
    .execute(&mut *conn)
    .await?;
    insert_outbox_event(
        conn,
        env_id,
        events::OFFICE_SUMMARY,
        serde_json::json!({"office_id": s.office_id, "total_net": s.total_net}),
    )
    .await
}

/// Session-close carry: write today's opening balances from the
/// engine snapshots for tomorrow's carry-in.
pub async fn persist_opening_balances(
    conn: &mut PgConnection,
    env_id: Uuid,
    positions: &[Position],
) -> Result<usize, sqlx::Error> {
    // context is transaction-local: set it on the executor this fn receives (DESIGN.md §5).
    sentin8::shell::set_tenant_context(conn, env_id).await;
    let tomorrow = chrono::Utc::now()
        .date_naive()
        .succ_opt()
        .unwrap_or_default();
    let mut n = 0usize;
    for p in positions {
        sqlx::query(
            r#"INSERT INTO pnl.opening_balance
               (env_id, trading_date, usr_id, office_id, symbol_id,
                signed_size, avg_cost_raw, price_scale, realized_carry)
               VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
               ON CONFLICT (env_id, trading_date, usr_id, symbol_id) DO NOTHING"#,
        )
        .bind(env_id)
        .bind(tomorrow)
        .bind(&p.usr_id)
        .bind(&p.office_id)
        .bind(p.symbol_id)
        .bind(p.signed_size)
        .bind(p.avg_cost_raw as i64)
        .bind(p.price_scale as i16)
        .bind(p.realized as i64)
        .execute(&mut *conn)
        .await?;
        n += 1;
    }
    Ok(n)
}

/// Load today's opening balances (carry-in at boot).
pub async fn load_opening_balances(
    conn: &mut PgConnection,
    env_id: Uuid,
) -> Result<Vec<(String, String, Uuid, i64, i64, u8, i64)>, sqlx::Error> {
    sentin8::shell::set_tenant_context(conn, env_id).await;
    let today = chrono::Utc::now().date_naive();
    let rows: Vec<(String, String, Uuid, i64, i64, i8, i64)> = sqlx::query_as(
        r#"SELECT usr_id, office_id, symbol_id, signed_size, avg_cost_raw,
                  price_scale::int8, realized_carry
           FROM pnl.opening_balance
           WHERE env_id = $1 AND trading_date = $2"#,
    )
    .bind(env_id)
    .bind(today)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(
                usr_id,
                office_id,
                symbol_id,
                signed_size,
                avg_cost_raw,
                price_scale,
                realized_carry,
            )| {
                (
                    usr_id,
                    office_id,
                    symbol_id,
                    signed_size,
                    avg_cost_raw,
                    price_scale.max(0) as u8,
                    realized_carry,
                )
            },
        )
        .collect())
}

/// Trader board read (the DB day rows served at /v1/pnl/traders).
pub async fn read_trader_board(
    conn: &mut PgConnection,
    env_id: Uuid,
    limit: i64,
) -> Result<Vec<(String, String, i64, i64, i64, i64)>, sqlx::Error> {
    sentin8::shell::set_tenant_context(conn, env_id).await;
    let rows: Vec<(String, String, i64, i64, i64, i64)> = sqlx::query_as(
        r#"SELECT usr_id, office_id, total_net, total_gross, total_fees, unrealized
           FROM pnl.trader_summary
           WHERE env_id = $1 AND trading_date = CURRENT_DATE
           ORDER BY total_net LIMIT $2"#,
    )
    .bind(env_id)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows)
}

/// Overnight mismatch event — the reconcile queue's input.
pub async fn persist_overnight_mismatch(
    conn: &mut PgConnection,
    env_id: Uuid,
    usr_id: &str,
    symbol_id: Uuid,
    expected: i64,
    actual: i64,
) -> Result<(), sqlx::Error> {
    insert_outbox_event(
        conn,
        env_id,
        events::OVERNIGHT_MISMATCH,
        serde_json::json!({
            "usr_id": usr_id, "symbol_id": symbol_id,
            "expected": expected, "actual": actual,
        }),
    )
    .await
}

/// Load currency rates (fee conversion at `desk.currency`'s
/// ×10⁴ rate_to_usd). The fill consumer folds per-currency fees into
/// the engine's single fee_total at USD scale.
pub async fn load_currency_rates(
    conn: &mut PgConnection,
    env_id: Uuid,
) -> Result<Vec<(String, i128)>, sqlx::Error> {
    sentin8::shell::set_tenant_context(conn, env_id).await;
    let rows: Vec<(String, i64)> =
        sqlx::query_as(r#"SELECT currency_code, rate_to_usd FROM desk.currency WHERE env_id = $1"#)
            .bind(env_id)
            .fetch_all(&mut *conn)
            .await?;
    Ok(rows.into_iter().map(|(c, r)| (c, r as i128)).collect())
}

/// Outbox insert — drained by the shell's boot-step-8b relay to Bus.
async fn insert_outbox_event(
    conn: &mut PgConnection,
    env_id: Uuid,
    event_type: &str,
    payload: serde_json::Value,
) -> Result<(), sqlx::Error> {
    let row_id =
        sentin8::bus::outbox::insert_outbox_row(&mut *conn, env_id, "pnl", event_type, &payload)
            .await
            .map_err(|e| match e {
                sentin8::bus::outbox::OutboxError::Db(db) => db,
                other => sqlx::Error::Configuration(other.to_string().into()),
            })?;
    let _ = row_id;
    Ok(())
}
