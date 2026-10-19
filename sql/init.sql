-- SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
-- SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
-- SPDX-FileCopyrightText: Copyright Contributors to the Trade8:Pnl
-- SPDX-License-Identifier: Apache-2.0

-- Trade8:Pnl - positions, day P&L, overnight carry.
--
-- Design: ../pnl/DESIGN.md §5.
--
-- order/PnL DB pattern - everything here is derived from fill events).

CREATE SCHEMA IF NOT EXISTS pnl;

CREATE TABLE IF NOT EXISTS pnl.opening_balance (
 env_id UUID NOT NULL,
 trading_date DATE NOT NULL,
 usr_id TEXT NOT NULL,
 office_id TEXT NOT NULL,
 symbol_id UUID NOT NULL,
 signed_size BIGINT NOT NULL,
 avg_cost_raw BIGINT NOT NULL,
 price_scale SMALLINT NOT NULL DEFAULT 4,
 realized_carry BIGINT NOT NULL DEFAULT 0,
 at TIMESTAMPTZ NOT NULL DEFAULT now(),
 PRIMARY KEY (env_id, trading_date, usr_id, symbol_id)
);

CREATE TABLE IF NOT EXISTS pnl.position_snapshot (
 env_id UUID NOT NULL,
 usr_id TEXT NOT NULL,
 office_id TEXT NOT NULL,
 symbol_id UUID NOT NULL,
 signed_size BIGINT NOT NULL,
 avg_cost_raw BIGINT NOT NULL,
 price_scale SMALLINT NOT NULL DEFAULT 4,
 realized BIGINT NOT NULL DEFAULT 0,
 unrealized BIGINT NOT NULL DEFAULT 0,
 fees BIGINT NOT NULL DEFAULT 0,
 updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
 PRIMARY KEY (env_id, usr_id, symbol_id)
);

CREATE TABLE IF NOT EXISTS pnl.symbol_resume (
 env_id UUID NOT NULL,
 usr_id TEXT NOT NULL,
 symbol_id UUID NOT NULL,
 trading_date DATE NOT NULL,
 gross BIGINT NOT NULL DEFAULT 0,
 fees BIGINT NOT NULL DEFAULT 0,
 net BIGINT NOT NULL DEFAULT 0,
 shares BIGINT NOT NULL DEFAULT 0,
 max_net BIGINT NOT NULL DEFAULT 0,
 lft BIGINT NOT NULL DEFAULT 0,
 unrealized BIGINT NOT NULL DEFAULT 0,
 PRIMARY KEY (env_id, usr_id, symbol_id, trading_date)
);

CREATE TABLE IF NOT EXISTS pnl.trader_summary (
 env_id UUID NOT NULL,
 usr_id TEXT NOT NULL,
 office_id TEXT NOT NULL,
 trading_date DATE NOT NULL,
 total_fees BIGINT NOT NULL DEFAULT 0,
 total_gross BIGINT NOT NULL DEFAULT 0,
 total_net BIGINT NOT NULL DEFAULT 0,
 total_shares BIGINT NOT NULL DEFAULT 0,
 max_net_day BIGINT NOT NULL DEFAULT 0,
 unrealized BIGINT NOT NULL DEFAULT 0,
 lft BIGINT NOT NULL DEFAULT 0,
 PRIMARY KEY (env_id, usr_id, trading_date)
);

CREATE TABLE IF NOT EXISTS pnl.office_summary (
 env_id UUID NOT NULL,
 office_id TEXT NOT NULL,
 trading_date DATE NOT NULL,
 total_fees BIGINT NOT NULL DEFAULT 0,
 total_gross BIGINT NOT NULL DEFAULT 0,
 total_net BIGINT NOT NULL DEFAULT 0,
 total_shares BIGINT NOT NULL DEFAULT 0,
 trader_count INT NOT NULL DEFAULT 0,
 PRIMARY KEY (env_id, office_id, trading_date)
);

-- ---------------------------------------------------------------------------
-- RLS
-- ---------------------------------------------------------------------------
DO $$
DECLARE tbl TEXT;
DECLARE pol TEXT;
BEGIN
 FOREACH tbl IN ARRAY ARRAY[
 'pnl.opening_balance', 'pnl.position_snapshot',
 'pnl.symbol_resume', 'pnl.trader_summary',
 'pnl.office_summary'
 ] LOOP
 BEGIN
 pol := replace(split_part(tbl, '.', 2), '"', '');
 EXECUTE format('ALTER TABLE %s ENABLE ROW LEVEL SECURITY', tbl);
 EXECUTE format('ALTER TABLE %s FORCE ROW LEVEL SECURITY', tbl);
 -- Suffix BEFORE %I quoting: `limit` alone is fine but the concatenation
 -- `"limit"_select` is invalid SQL - build 'limit_select' unquoted
 -- (desk-store DO-block memory: reserved-word table names are the
 -- classic failure class).
 EXECUTE format('DROP POLICY IF EXISTS %1$I ON %2$s;
CREATE POLICY %1$I ON %2$s FOR SELECT USING (sentin8_context_initialized() AND env_id = sentin8_current_env_id())', pol || '_select', tbl);
 EXECUTE format('DROP POLICY IF EXISTS %1$I ON %2$s;
CREATE POLICY %1$I ON %2$s FOR INSERT WITH CHECK (sentin8_context_initialized() AND env_id = sentin8_current_env_id())', pol || '_insert', tbl);
 EXECUTE format('DROP POLICY IF EXISTS %1$I ON %2$s;
CREATE POLICY %1$I ON %2$s FOR UPDATE USING (sentin8_context_initialized() AND env_id = sentin8_current_env_id())', pol || '_update', tbl);
 EXECUTE format('DROP POLICY IF EXISTS %1$I ON %2$s;
CREATE POLICY %1$I ON %2$s FOR DELETE USING (sentin8_context_initialized() AND env_id = sentin8_current_env_id())', pol || '_delete', tbl);
 EXCEPTION WHEN OTHERS THEN
 RAISE NOTICE 'RLS setup failed for %: %', tbl, SQLERRM;
 END;
 END LOOP;
END $$;

-- Local outbox + audit journal (DBX §6.7 / §6.8 - shell-generated DDL).
CREATE TABLE IF NOT EXISTS pnl.outbox (
 id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
 env_id UUID NOT NULL,
 event_type TEXT NOT NULL,
 stream_family TEXT NOT NULL,
 payload JSONB NOT NULL,
 publisher_id_token TEXT,
 publisher_subject TEXT,
 relayed BOOLEAN NOT NULL DEFAULT false,
 relayed_at TIMESTAMPTZ,
 relay_attempts INT NOT NULL DEFAULT 0,
    next_retry_at   TIMESTAMPTZ,
 created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_outbox_unrelayed
 ON pnl.outbox (created_at) WHERE relayed = false;
ALTER TABLE pnl.outbox ENABLE ROW LEVEL SECURITY;
ALTER TABLE pnl.outbox FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS outbox_select ON pnl.outbox;
CREATE POLICY outbox_select ON pnl.outbox FOR SELECT
 USING (sentin8_context_initialized() AND env_id = sentin8_current_env_id());
DROP POLICY IF EXISTS outbox_insert ON pnl.outbox;
CREATE POLICY outbox_insert ON pnl.outbox FOR INSERT
 WITH CHECK (sentin8_context_initialized() AND env_id = sentin8_current_env_id());
DROP POLICY IF EXISTS outbox_update ON pnl.outbox;
CREATE POLICY outbox_update ON pnl.outbox FOR UPDATE
 USING (sentin8_context_initialized() AND env_id = sentin8_current_env_id());
CREATE OR REPLACE FUNCTION pnl.pnl_notify_outbox() RETURNS TRIGGER
LANGUAGE plpgsql AS $fn$
BEGIN
 PERFORM pg_notify('pnl_outbox_changed', '');
 RETURN NEW;
END;
$fn$;
DROP TRIGGER IF EXISTS outbox_notify_trigger ON pnl.outbox;
CREATE TRIGGER outbox_notify_trigger
 AFTER INSERT ON pnl.outbox
 FOR EACH ROW
 EXECUTE FUNCTION pnl.pnl_notify_outbox();

CREATE TABLE IF NOT EXISTS pnl.audit_journal (
 id UUID PRIMARY KEY,
 env_id UUID NOT NULL,
 action TEXT NOT NULL,
 target_resource_type TEXT,
 target_resource_id TEXT,
 actor_user_id TEXT,
 actor_subject_type TEXT,
 severity TEXT NOT NULL DEFAULT 'info',
 correlation_id TEXT,
 before_state JSONB,
 after_state JSONB,
 details JSONB,
 pushed BOOLEAN NOT NULL DEFAULT false,
 pushed_at TIMESTAMPTZ,
 created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_pnl_audit_journal_unpushed
 ON pnl.audit_journal (created_at) WHERE pushed = false;
ALTER TABLE pnl.audit_journal ENABLE ROW LEVEL SECURITY;
ALTER TABLE pnl.audit_journal FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS audit_journal_dml ON pnl.audit_journal;
CREATE POLICY audit_journal_dml ON pnl.audit_journal
 USING (sentin8_context_initialized() AND env_id = sentin8_current_env_id())
 WITH CHECK (sentin8_context_initialized() AND env_id = sentin8_current_env_id());

-- ---------------------------------------------------------------------------
-- Per-service role grants + search_path (bank8-ledger pattern; the CLI
-- creates the role BEFORE this SQL - grant-before-role incident rule).
-- Grants live in db trade8 (this database); the scope→role map row is
-- upserted by `vertical enroll` from --section=pg_roles.
-- ---------------------------------------------------------------------------
DO $$
BEGIN
 IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'trade8_pnl_service') THEN
  ALTER ROLE trade8_pnl_service SET search_path = pnl, public;
  GRANT USAGE ON SCHEMA pnl TO trade8_pnl_service;
  GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA pnl TO trade8_pnl_service;
  GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA pnl TO trade8_pnl_service;
  ALTER DEFAULT PRIVILEGES FOR ROLE sentin8 IN SCHEMA pnl
   GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO trade8_pnl_service;
  ALTER DEFAULT PRIVILEGES FOR ROLE sentin8 IN SCHEMA pnl
   GRANT USAGE, SELECT ON SEQUENCES TO trade8_pnl_service;
 END IF;
END $$;
-- Declared cross-schema reads (SELECT-only) - mirrors registration.rs
-- PgRole.cross_schema_grants; `vertical enroll` owns their application,
-- this block only re-asserts them idempotently after a role recreation.
DO $$ BEGIN
 IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'trade8_pnl_service') THEN
  GRANT USAGE ON SCHEMA desk TO trade8_pnl_service;
  GRANT SELECT ON desk.currency TO trade8_pnl_service;
 END IF;
END $$;
