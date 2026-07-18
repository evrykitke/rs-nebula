-- Phase 5: instrumentation and count discipline.
--
-- pos_settings is the tenant's POS behaviour, one row per tenant database
-- (the CHECK-ed boolean key is the singleton lock). blind_count hides the
-- expected cash from the cashier until they have counted; denominations
-- is the note/coin set the count sheet offers, defaulting to KES.

CREATE TABLE pos_settings (
    id              BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (id),
    blind_count     BOOLEAN NOT NULL DEFAULT FALSE,
    denominations   JSONB NOT NULL DEFAULT '[1000, 500, 200, 100, 50, 20, 10, 5, 1]'::jsonb,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by      UUID
);

-- What the till measured about the sale itself: how long it took from
-- first line to payment, and how many inputs (taps/keys/scans) it cost.
-- Client-reported, nullable — an older till simply doesn't say.
ALTER TABLE pos_orders
    ADD COLUMN capture_seconds INTEGER,
    ADD COLUMN input_count     INTEGER;

-- The session's instrumentation, aggregated once at close so the session
-- record answers "how fast was this till" without rescanning its orders.
ALTER TABLE pos_sessions
    ADD COLUMN avg_sale_seconds NUMERIC(10, 2),
    ADD COLUMN avg_sale_inputs  NUMERIC(10, 2),
    ADD COLUMN void_count       INTEGER;

-- The denomination breakdown behind a counted amount, when the count
-- sheet was used: [{"denom": "1000", "count": 3}, ...]. Stored so the Z
-- report shows how the drawer was counted, not just what it summed to.
ALTER TABLE pos_session_counts
    ADD COLUMN denominations JSONB;
