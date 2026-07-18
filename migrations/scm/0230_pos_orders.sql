-- The lightweight sale document. kind: sale | refund (refund lines carry
-- the original order/line refs). status: captured | voided — a captured
-- order is immutable; corrections are refunds. client_uuid is the
-- offline idempotency key: unique, generated at the till.

CREATE TABLE pos_orders (
    id              UUID PRIMARY KEY,
    number          TEXT,                    -- RCP-, allocated at capture/sync
    client_uuid     UUID NOT NULL,
    session_id      UUID NOT NULL REFERENCES pos_sessions (id),
    kind            TEXT NOT NULL DEFAULT 'sale',
    customer_id     UUID NOT NULL,           -- walk-in unless attached
    sold_at         TIMESTAMPTZ NOT NULL,    -- client-captured moment
    currency        TEXT NOT NULL,
    -- Totals are stored, not recomputed: the receipt is a legal fact.
    -- All amounts are tax-inclusive retail money; tax_total is the VAT
    -- inside total, not on top of it.
    subtotal        NUMERIC(20, 4) NOT NULL,
    discount_total  NUMERIC(20, 4) NOT NULL DEFAULT 0,
    tax_total       NUMERIC(20, 4) NOT NULL DEFAULT 0,
    total           NUMERIC(20, 4) NOT NULL,
    refund_of_id    UUID REFERENCES pos_orders (id),
    -- Sync bookkeeping:
    captured_offline BOOLEAN NOT NULL DEFAULT FALSE,
    price_drift     BOOLEAN NOT NULL DEFAULT FALSE,  -- synced price differs from current price (Z report flag)
    status          TEXT NOT NULL DEFAULT 'captured',
    voided_at       TIMESTAMPTZ,
    voided_by       UUID,
    void_reason     TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by      UUID                      -- the cashier
);
CREATE UNIQUE INDEX ux_pos_orders_number ON pos_orders (number);
CREATE UNIQUE INDEX ux_pos_orders_client_uuid ON pos_orders (client_uuid);
CREATE INDEX ix_pos_orders_session ON pos_orders (session_id);
CREATE INDEX ix_pos_orders_customer ON pos_orders (customer_id);
CREATE INDEX ix_pos_orders_refund_of ON pos_orders (refund_of_id);

CREATE TABLE pos_order_lines (
    id           UUID PRIMARY KEY,
    order_id     UUID NOT NULL REFERENCES pos_orders (id) ON DELETE CASCADE,
    line_no      INTEGER NOT NULL,
    item_id      UUID NOT NULL,              -- inventory_items, soft ref
    description  TEXT NOT NULL,              -- snapshot: receipts outlive renames
    qty          NUMERIC(20, 6) NOT NULL,    -- positive; kind says direction
    unit_price   NUMERIC(20, 6) NOT NULL,    -- tax-inclusive retail price
    price_source TEXT,                       -- list:{uuid} | item_default | manual
    discount_pct NUMERIC(7, 4),
    tax_code_id  UUID,
    tax_amount   NUMERIC(20, 4) NOT NULL DEFAULT 0,
    net          NUMERIC(20, 4) NOT NULL,    -- what the customer pays for the line (incl. tax)
    batch_id     UUID,                       -- captured when the item tracks batches
    refund_of_line_id UUID REFERENCES pos_order_lines (id),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_pos_order_lines_order ON pos_order_lines (order_id);
CREATE INDEX ix_pos_order_lines_item ON pos_order_lines (item_id);
CREATE INDEX ix_pos_order_lines_refund_of ON pos_order_lines (refund_of_line_id);

-- Tenders: one order, many payment lines (split payments).
-- tender: cash | mpesa | card. reference: M-Pesa code / card slip no.
CREATE TABLE pos_order_payments (
    id           UUID PRIMARY KEY,
    order_id     UUID NOT NULL REFERENCES pos_orders (id) ON DELETE CASCADE,
    tender       TEXT NOT NULL,
    amount       NUMERIC(20, 4) NOT NULL,    -- amount applied to the sale
    tendered     NUMERIC(20, 4),             -- cash given; change = tendered - amount
    reference    TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_pos_order_payments_order ON pos_order_payments (order_id);
