// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Trade8:Pnl
// SPDX-License-Identifier: Apache-2.0

//! Trade8:Pnl service binary — day P&L engine (bus consumer).
//!
//! Design: `../pnl/DESIGN.md`.
//! Fills/marks drive the
//! engine; positions/resumes/summaries upsert to the DB with
//! `position:updated`/`trader:summary`/`office:summary` outbox events;
//! session-close writes tomorrow's opening balances — everything
//! derives from fill events + opening balances (no dual-write).

#![allow(dead_code)]

// Global allocator (design §11 Allocator): mimalloc in every trade8
// service/worker binary for lower tail latency than the system allocator.
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod admin_ui;
mod consumers;
mod core;
mod registration;
mod store;

use sqlx::Acquire;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};

use trade8::contracts::{Fill, FillState};

/// Service state.
pub struct PnlState {
    pub core: sentin8::shell::ServiceCore,
    pub engine: std::sync::Arc<core::PnlEngine>,
    /// Fill/mark fan-out input (Desk/Router events → consumers).
    pub tx: tokio::sync::broadcast::Sender<String>,
}

fn extend_router(app: Router<Arc<PnlState>>, _state: Arc<PnlState>) -> Router<Arc<PnlState>> {
    app.route("/v1/pnl/summary", get(self_summary))
        .route("/v1/pnl/positions", get(self_positions))
        .route("/v1/pnl/office", get(office_summary))
        .route("/v1/pnl/traders", get(trader_board))
        .route("/v1/pnl/session-close", post(session_close))
        // Admin RFW pages.
        .route("/v1/admin/ui/{*page}", get(admin_ui::get_admin_ui))
        // ── Shield policy-admin API (shared
        // implementation; handlers generic over this state via HasPolicyAdmin).
        .route(
            "/v1/admin/policy-sets",
            get(sentin8::shield::admin::list_policy_sets::<Arc<PnlState>>),
        )
        .route(
            "/v1/admin/policy-sets/{name}",
            get(sentin8::shield::admin::get_policy_set::<Arc<PnlState>>)
                .put(sentin8::shield::admin::put_policy_set::<Arc<PnlState>>)
                .delete(sentin8::shield::admin::delete_policy_set::<Arc<PnlState>>),
        )
        .route(
            "/v1/admin/policy-sets/{name}/test",
            post(sentin8::shield::admin::test_policy_set::<Arc<PnlState>>),
        )
        .route(
            "/v1/admin/policy-sets/enforce",
            post(sentin8::shield::admin::admin_enforce_policy_set::<Arc<PnlState>>),
        )
        .route(
            "/v1/admin/policy-sets/shadow",
            post(sentin8::shield::admin::admin_shadow_policy_set::<Arc<PnlState>>),
        )
        .route(
            "/v1/admin/policy-sets/disable",
            post(sentin8::shield::admin::admin_disable_policy_set::<Arc<PnlState>>),
        )
        .route(
            "/v1/admin/relations",
            get(sentin8::shield::admin::list_relations::<Arc<PnlState>>)
                .put(sentin8::shield::admin::put_relation::<Arc<PnlState>>)
                .delete(sentin8::shield::admin::delete_relation::<Arc<PnlState>>),
        )
        .route(
            "/v1/admin/relations/check",
            get(sentin8::shield::admin::check_relation::<Arc<PnlState>>),
        )
        .route(
            "/v1/admin/attributes",
            get(sentin8::shield::admin::list_attributes::<Arc<PnlState>>)
                .put(sentin8::shield::admin::put_attribute::<Arc<PnlState>>)
                .delete(sentin8::shield::admin::delete_attribute::<Arc<PnlState>>),
        )
        .route("/v1/admin/traders", get(admin_ui::admin_traders))
        .route("/v1/admin/overnight", get(admin_ui::admin_overnight))
        .route(
            "/v1/health",
            get(|| async { Json(serde_json::json!({"status": "ok", "service": "trade8:pnl"})) }),
        )
}

async fn self_summary(
    State(st): State<Arc<PnlState>>,
    headers: axum::http::HeaderMap,
) -> Result<Json<trade8::contracts::TraderPnlSummary>, (axum::http::StatusCode, String)> {
    let auth = sentin8::shell::verify_token(&st.core.config, &headers).await?;
    sentin8::shell::policy::require(
        &st.core.admin.policy,
        &auth,
        &headers,
        sentin8::shield::EvalTarget::new("trade8:pnl:self:read"),
    )
    .await?;
    Ok(Json(st.engine.summary(&auth.claims.sub)))
}

async fn self_positions(
    State(st): State<Arc<PnlState>>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Vec<trade8::contracts::Position>>, (axum::http::StatusCode, String)> {
    let auth = sentin8::shell::verify_token(&st.core.config, &headers).await?;
    sentin8::shell::policy::require(
        &st.core.admin.policy,
        &auth,
        &headers,
        sentin8::shield::EvalTarget::new("trade8:pnl:self:read"),
    )
    .await?;
    Ok(Json(st.engine.positions_for(&auth.claims.sub)))
}

async fn office_summary(
    State(st): State<Arc<PnlState>>,
    headers: axum::http::HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<trade8::contracts::OfficePnlSummary>, (axum::http::StatusCode, String)> {
    let _a = sentin8::shell::verify_token(&st.core.config, &headers).await?;
    sentin8::shell::policy::require(
        &st.core.admin.policy,
        &_a,
        &headers,
        sentin8::shield::EvalTarget::new("trade8:pnl:office:read"),
    )
    .await?;
    let office = q
        .get("office_id")
        .cloned()
        .unwrap_or_else(|| "DEFAULT".into());
    Ok(Json(st.engine.office_summary(&office)))
}

/// The desk board (manager scope): today's trader summary rows from the
/// DB (source of truth — the persist pass).
async fn trader_board(
    State(st): State<Arc<PnlState>>,
    headers: axum::http::HeaderMap,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let a = sentin8::shell::verify_token(&st.core.config, &headers).await?;
    sentin8::shell::policy::require(
        &st.core.admin.policy,
        &a,
        &headers,
        sentin8::shield::EvalTarget::new("trade8:pnl:env:read"),
    )
    .await?;
    let env = a.env_id();
    let mut conn = st
        .core
        .pool
        .acquire()
        .await
        .map_err(sentin8::shell::err_db)?;
    sentin8::shell::set_tenant_context(&mut conn, env).await;
    // RLS context is transaction-local (DESIGN.md §5) —
    // begin() pins the tenant context to the SAME tx as the DML/reads.
    let mut tx = match conn.begin().await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "pnl: tenant tx begin failed");
            return Err(sentin8::shell::err_db(e));
        }
    };
    let rows = store::read_trader_board(&mut tx, env, 100)
        .await
        .map_err(sentin8::shell::err_db)?;
    tx.commit().await.ok(); // dropping an uncommitted tx rolls back
    Ok(Json(serde_json::json!({"traders": rows.into_iter().map(
        |(usr_id, office_id, total_net, total_gross, total_fees, unrealized)| {
            serde_json::json!({
                "usr_id": usr_id, "office_id": office_id,
                "total_net": total_net, "total_gross": total_gross,
                "total_fees": total_fees, "unrealized": unrealized,
            })
        }
    ).collect::<Vec<_>>()})))
}

/// Session close (the Cron proxy-back target): persist all
/// positions/resumes/summaries + tomorrow's opening balances.
async fn session_close(
    State(st): State<Arc<PnlState>>,
    headers: axum::http::HeaderMap,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let a = sentin8::shell::verify_token(&st.core.config, &headers).await?;
    sentin8::shell::policy::require(
        &st.core.admin.policy,
        &a,
        &headers,
        sentin8::shield::EvalTarget::new("trade8:pnl:env:read"),
    )
    .await?;
    let env = a.env_id();
    let mut conn = st
        .core
        .pool
        .acquire()
        .await
        .map_err(sentin8::shell::err_db)?;
    sentin8::shell::set_tenant_context(&mut conn, env).await;
    // RLS context is transaction-local (DESIGN.md §5) —
    // begin() pins the tenant context to the SAME tx as the DML/reads.
    let mut tx = match conn.begin().await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "pnl: tenant tx begin failed");
            return Err(sentin8::shell::err_db(e));
        }
    };
    let trading_date = chrono::Utc::now().date_naive().to_string();
    // All distinct traders from the engine book.
    let traders: Vec<String> = {
        let mut v: Vec<String> = st
            .engine
            .positions_all()
            .into_iter()
            .map(|p| p.usr_id)
            .collect();
        v.sort();
        v.dedup();
        v
    };
    let mut positions_persisted = 0usize;
    for usr in &traders {
        for p in st.engine.positions_for(usr) {
            if store::persist_position(&mut tx, env, &trading_date, &p)
                .await
                .is_ok()
            {
                positions_persisted += 1;
            }
            let resume = st.engine.resume_for(usr, p.symbol_id);
            let _ =
                store::persist_resume(&mut tx, env, &trading_date, usr, p.symbol_id, &resume).await;
        }
        let summary = st.engine.summary(usr);
        let _ = store::persist_trader_summary(&mut tx, env, &trading_date, &summary).await;
        let office = st.engine.office_summary(&summary.office_id);
        let _ = store::persist_office_summary(&mut tx, env, &trading_date, &office).await;
    }
    let all_positions = st.engine.positions_all();
    let balances = store::persist_opening_balances(&mut tx, env, &all_positions)
        .await
        .unwrap_or(0);
    tx.commit().await.ok(); // dropping an uncommitted tx rolls back
    tracing::info!(
        traders = traders.len(),
        positions_persisted,
        balances,
        "session close persisted"
    );
    Ok(Json(serde_json::json!({
        "traders": traders.len(),
        "positions_persisted": positions_persisted,
        "opening_balances": balances,
    })))
}

/// The fill consumer: Desk/Router fill events drive the engine;
/// marks from prints refresh unrealized. Broadcast input.
async fn run_fill_consumer(state: Arc<PnlState>, _env_id: uuid::Uuid) {
    let mut rx = state.tx.subscribe();
    while let Ok(msg) = rx.recv().await {
        if let Ok(fill) = serde_json::from_str::<Fill>(&msg) {
            if fill.state != FillState::Fill {
                continue;
            }
            // The Desk event envelope carries the resolved context.
            let usr = fill.gateway_order_id.clone().unwrap_or_default();
            let _ = usr;
            // Full context arrives via the bus envelope in the trigger
            // pass; the broadcast path drives the demo loop.
            state.engine.apply_fill(&fill, "demo", "HO", "DEMO");
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    sentin8::shell::serve(
        sentin8::shell::Sentin8Shell::new("trade8:pnl", "TRADE8_PNL")
            .with_registration(registration::print)
            .with_own_health()
            .with_state(|core| async move {
                let engine = core::PnlEngine::default();
                let engine = std::sync::Arc::new(engine);
                engine.set_trading_date(&chrono::Utc::now().date_naive().to_string());
                let (tx, _) = tokio::sync::broadcast::channel(1024);
                let state = Arc::new(PnlState { core, engine, tx });
                // Boot pass: carry in today's opening balances.
                if let Ok(env_slug) = std::env::var("TRADE8_PNL_ENV") {
                    if let Ok(env_id) = uuid::Uuid::parse_str(&env_slug) {
                        if let Ok(mut conn) = state.core.pool.acquire().await {
                            sentin8::shell::set_tenant_context(&mut conn, env_id).await;
                            // RLS context is transaction-local (DESIGN.md §5) —
                            // begin() pins the tenant context to the SAME tx as the DML/reads.
                            let Ok(mut tx) = conn.begin().await else {
                                tracing::warn!("pnl: boot tx begin failed — carry-in skipped");
                                return Ok::<_, anyhow::Error>(state.clone());
                            };
                            match store::load_opening_balances(&mut tx, env_id).await {
                                Ok(rows) => {
                                    let carried = rows.len();
                                    for (usr, office, _symbol, _size, _cost, _scale, _carry) in rows
                                    {
                                        state.engine.set_office(&usr, &office);
                                    }
                                    tracing::info!(carried, "opening balances carried in");
                                }
                                Err(e) => tracing::warn!(error = %e, "opening balance load failed"),
                            }
                        }
                        let st2 = state.clone();
                        tokio::spawn(async move {
                            run_fill_consumer(st2, env_id).await;
                        });
                    }
                }
                // Bus consumers (pnl design §7): router fills →
                // positions/fees/summaries; desk session-close persist +
                // overnight verification; feed prints → marks.
                consumers::spawn_consumers(
                    state.core.pool.clone(),
                    &state.core.config.sentin8_bus_url,
                    &state.core.config.sentin8_auth_issuer,
                    state.engine.clone(),
                );
                Ok(state)
            })
            .with_router(extend_router),
    )
    .await
}

/// Shared policy-admin surface accessor: the shell boot
/// loaded the engine into `core.admin`; the shared handlers extract it via
/// `HasPolicyAdmin` so the admin surface and the request path evaluate the
/// SAME cache.
impl sentin8::shield::admin::HasPolicyAdmin for PnlState {
    fn policy_admin(&self) -> &sentin8::shield::admin::SharedAdminState {
        &self.core.admin
    }
}

#[cfg(test)]
// Latency harness — fill→summary budget (1k fills < 10s CI headroom;
// the design budget number is measured release-mode).
mod latency {
    use super::*;
    use std::time::Instant;

    #[test]
    fn pnl_fill_to_summary_latency() {
        let e = core::PnlEngine::default();
        e.set_trading_date("2026-09-03");
        let sym = uuid::Uuid::new_v4();
        let start = Instant::now();
        for i in 0..1_000 {
            e.apply_fill_context(
                "t1",
                "HO",
                sym,
                "AAPL",
                if i % 2 == 0 { 'B' } else { 'S' },
                100,
                100_000 + (i % 10) as i128 * 100,
                4,
                0,
            );
            let _ = e.summary("t1");
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() < 10_000,
            "1k fill→summary took {:?}",
            elapsed
        );
        println!("pnl fill→summary: 1k fills, {}ms", elapsed.as_millis());
    }
}
