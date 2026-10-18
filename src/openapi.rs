// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Trade8:Pnl
// SPDX-License-Identifier: Apache-2.0

//! OpenAPI spec for Trade8:Pnl.
//!
//! utoipa annotations on the binary's route handlers + the shared Shield
//! policy-admin surface (`sentin8::shell::policy_*` wrappers). See
//! `sentin8/design/sentin8-openapi-coverage-design.md` §2.

use utoipa::OpenApi;

pub mod w {


    /// Spec wrapper for the inline health closure (binary has no named fn).
    #[utoipa::path(
        get,
        path = "/v1/health",
        responses(
            (status = 200, description = "Service is healthy", body = serde_json::Value),
        ),
        security(),
        tag = "pnl",
    )]
    pub async fn pnl_health() {}

}

/// OpenAPI spec for the pnl API.
#[derive(OpenApi)]
#[openapi(
    paths(
crate::self_summary,
crate::self_positions,
crate::office_summary,
crate::trader_board,
crate::session_close,
crate::admin_ui::get_admin_ui,
crate::admin_ui::admin_traders,
crate::admin_ui::admin_overnight,
crate::openapi::w::pnl_health,
        sentin8::shell::policy_list_sets,
        sentin8::shell::policy_get_set,
        sentin8::shell::policy_put_set,
        sentin8::shell::policy_delete_set,
        sentin8::shell::policy_test_set,
        sentin8::shell::policy_mode_enforce,
        sentin8::shell::policy_mode_shadow,
        sentin8::shell::policy_mode_disable,
        sentin8::shell::policy_list_relations,
        sentin8::shell::policy_put_relation,
        sentin8::shell::policy_delete_relation,
        sentin8::shell::policy_check_relation,
        sentin8::shell::policy_list_attributes,
        sentin8::shell::policy_put_attribute,
        sentin8::shell::policy_delete_attribute,
    ),
    components(schemas()),
    security(),
    tags(
        (name = "pnl", description = "Trade8:PnL — profit & loss"),
        (name = "trade8-admin", description = "Trade8 admin surface (RFW)"),
        (name = "policy", description = "Shield policy administration (shared surface)"),
    ),
)]
pub struct PnlOpenApi;
