// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Trade8:Pnl
// SPDX-License-Identifier: Apache-2.0

//! Trade8:Pnl admin UI — RFW pages + data endpoints.

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::Json;
use sentin8::rfw::page::AdminPage;
use sentin8::rfw::response::AdminUiResponse;
use sentin8::rfw::widget::*;

use crate::PnlState;

fn cols(key: &str, label: &str) -> WidgetDataTableColumn {
    WidgetDataTableColumn {
        key: key.into(),
        label: label.into(),
        sortable: true,
        width: None,
        format: None,
    }
}
fn col(key: &str, label: &str) -> WidgetDataTableColumn {
    WidgetDataTableColumn {
        key: key.into(),
        label: label.into(),
        sortable: false,
        width: None,
        format: None,
    }
}
fn table(columns: Vec<WidgetDataTableColumn>, endpoint: &str) -> RemoteWidget {
    RemoteWidget::DataTable(WidgetDataTable {
        columns,
        data_endpoint: endpoint.into(),
        sortable: true,
        filterable: true,
        page_size: 25,
        empty_message: Some("No data.".into()),
        row_link: None,
        row_actions: vec![],
    })
}
fn card(title: &str, body: &str) -> RemoteWidget {
    RemoteWidget::Card(WidgetCard {
        title: Some(title.into()),
        subtitle: None,
        children: vec![RemoteWidget::Text(WidgetText {
            text: body.into(),
            text_type: WidgetTextType::Body,
            align: TextAlign::Left,
            max_lines: None,
        })],
        actions: vec![],
    })
}
#[utoipa::path(
    get,
    path = "/v1/admin/ui/{page}",
    responses(
        (status = 200, description = "OK", body = serde_json::Value),
        (status = 401, description = "Not authenticated"),
        (status = 403, description = "Forbidden"),
    ),
    security(),
    tag = "trade8-admin",
)]

/// `GET /v1/admin/ui/{page}` — RFW definitions.
pub async fn get_admin_ui(
    State(_st): State<std::sync::Arc<PnlState>>,
    headers: HeaderMap,
    Path(page): Path<String>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let _a = sentin8::shell::verify_token(&_st.core.config, &headers).await?;
    sentin8::shell::policy::require(
        &_st.core.admin.policy,
        &_a,
        &headers,
        sentin8::shield::EvalTarget::new("trade8:pnl:office:read"),
    )
    .await?;
    let def = match page.as_str() {
        "traders" => AdminPage::new("Trader P&L Board")
            .widget(table(
                vec![
                    cols("usr_id", "Trader"),
                    cols("office_id", "Office"),
                    cols("total_net", "Net"),
                    cols("total_gross", "Gross"),
                    cols("total_fees", "Fees"),
                    cols("unrealized", "Unrealized"),
                ],
                "/v1/admin/traders",
            ))
            .build(),
        "overnight" => AdminPage::new("Overnight Reconciliation")
            .widget(card(
                "Reconciliation Queue",
                "Opening-balance mismatches from the overnight verification pass.",
            ))
            .widget(table(
                vec![
                    cols("usr_id", "Trader"),
                    cols("symbol_id", "Symbol"),
                    cols("expected", "Expected"),
                    cols("actual", "Actual"),
                ],
                "/v1/admin/overnight",
            ))
            .build(),
        _ => {
            return Err((
                axum::http::StatusCode::NOT_FOUND,
                format!("Unknown admin UI page: {page}"),
            ))
        }
    };
    Ok(Json(
        serde_json::to_value(AdminUiResponse::page(def)).unwrap_or_default(),
    ))
}

type Qp = std::collections::HashMap<String, String>;
#[utoipa::path(
    get,
    path = "/v1/admin/traders",
    responses(
        (status = 200, description = "OK", body = serde_json::Value),
        (status = 401, description = "Not authenticated"),
        (status = 403, description = "Forbidden"),
    ),
    security(),
    tag = "trade8-admin",
)]

/// `GET /v1/admin/traders` — today's trader board (DB rows).
pub async fn admin_traders(
    State(st): State<std::sync::Arc<PnlState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let a = sentin8::shell::verify_token(&st.core.config, &headers).await?;
    sentin8::shell::policy::require(
        &st.core.admin.policy,
        &a,
        &headers,
        sentin8::shield::EvalTarget::new("trade8:pnl:office:read"),
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
    let rows = crate::store::read_trader_board(&mut conn, env, 100)
        .await
        .map_err(sentin8::shell::err_db)?;
    Ok(Json(serde_json::json!(rows
        .into_iter()
        .map(
            |(usr_id, office_id, total_net, total_gross, total_fees, unrealized)| {
                serde_json::json!({
                    "usr_id": usr_id, "office_id": office_id,
                    "total_net": total_net, "total_gross": total_gross,
                    "total_fees": total_fees, "unrealized": unrealized,
                })
            }
        )
        .collect::<Vec<_>>())))
}
#[utoipa::path(
    get,
    path = "/v1/admin/overnight",
    responses(
        (status = 200, description = "OK", body = serde_json::Value),
        (status = 401, description = "Not authenticated"),
        (status = 403, description = "Forbidden"),
    ),
    security(),
    tag = "trade8-admin",
)]

/// `GET /v1/admin/overnight` — the reconcile queue feed. Mismatches are
/// transient (outbox events feed the ops queue); the wall shows the
/// opening-balance table for today with position-snapshot deltas.
pub async fn admin_overnight(
    State(st): State<std::sync::Arc<PnlState>>,
    headers: HeaderMap,
    Query(q): Query<Qp>,
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
    let limit = q
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(100)
        .min(500);
    let mut conn = st
        .core
        .pool
        .acquire()
        .await
        .map_err(sentin8::shell::err_db)?;
    sentin8::shell::set_tenant_context(&mut conn, env).await;
    let rows: Vec<(String, uuid::Uuid, i64, i64)> = sqlx::query_as(
        r#"SELECT ob.usr_id, ob.symbol_id, ob.signed_size,
                  COALESCE(ps.signed_size, 0)
           FROM pnl.opening_balance ob
           LEFT JOIN pnl.position_snapshot ps
             ON ps.env_id = ob.env_id AND ps.usr_id = ob.usr_id
            AND ps.symbol_id = ob.symbol_id
           WHERE ob.env_id = $1 AND ob.trading_date = CURRENT_DATE
           LIMIT $2"#,
    )
    .bind(env)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await
    .map_err(sentin8::shell::err_db)?;
    Ok(Json(serde_json::json!(rows
        .into_iter()
        .map(|(usr_id, symbol_id, expected, actual)| {
            serde_json::json!({
                "usr_id": usr_id, "symbol_id": symbol_id,
                "expected": expected, "actual": actual,
                "mismatch": expected != actual,
            })
        })
        .collect::<Vec<_>>())))
}
