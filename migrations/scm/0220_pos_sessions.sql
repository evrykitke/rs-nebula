-- One cashier's span at one register. status: open | closing | closed.
-- 'closing' exists because consolidation is a real posting step that can
-- fail and be retried without reopening the till to sales.

CREATE TABLE pos_sessions (
    id              UUID PRIMARY KEY,
    number          TEXT,                    -- PS-, allocated at open
    register_id     UUID NOT NULL REFERENCES pos_registers (id),
    cashier_id      UUID NOT NULL,           -- administration user, soft ref
    status          TEXT NOT NULL DEFAULT 'open',
    opened_at       TIMESTAMPTZ NOT NULL,
    opening_float   NUMERIC(20, 4) NOT NULL DEFAULT 0,
    closed_at       TIMESTAMPTZ,
    closed_by       UUID,
    closing_note    TEXT,                    -- required when over/short nonzero
    -- Consolidation artifacts, set as close completes:
    move_id         UUID,                    -- the aggregated stock movement
    gl_source       TEXT,                    -- outbox source key for the revenue entry
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX ux_pos_sessions_number ON pos_sessions (number);
-- One open (or still-closing) session per register.
CREATE UNIQUE INDEX ux_pos_sessions_open_register
    ON pos_sessions (register_id) WHERE status IN ('open', 'closing');
CREATE INDEX ix_pos_sessions_cashier ON pos_sessions (cashier_id, status);
CREATE INDEX ix_pos_sessions_register ON pos_sessions (register_id, opened_at);

-- Counted vs expected per tender at close; expected is computed and
-- stored at close time so the Z report is self-contained forever.
CREATE TABLE pos_session_counts (
    id           UUID PRIMARY KEY,
    session_id   UUID NOT NULL REFERENCES pos_sessions (id) ON DELETE CASCADE,
    tender       TEXT NOT NULL,              -- cash | mpesa | card
    expected     NUMERIC(20, 4) NOT NULL,
    counted      NUMERIC(20, 4) NOT NULL,
    UNIQUE (session_id, tender)
);

-- Paid-in / paid-out drawer events. kind: paid_in | paid_out.
CREATE TABLE pos_cash_movements (
    id           UUID PRIMARY KEY,
    session_id   UUID NOT NULL REFERENCES pos_sessions (id),
    kind         TEXT NOT NULL,
    amount       NUMERIC(20, 4) NOT NULL,
    reason       TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by   UUID
);
CREATE INDEX ix_pos_cash_movements_session ON pos_cash_movements (session_id);
