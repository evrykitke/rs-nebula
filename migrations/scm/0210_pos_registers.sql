-- A till: where sales happen, tied to the warehouse it sells from.
-- grid_layout is the per-register tile arrangement the POS client owns
-- and the server keeps; the server never interprets it.

CREATE TABLE pos_registers (
    id              UUID PRIMARY KEY,
    code            TEXT NOT NULL,
    name            TEXT NOT NULL,
    warehouse_id    UUID NOT NULL,           -- inventory_warehouses, soft ref
    price_list_id   UUID,                    -- overrides the walk-in default resolution
    default_customer_id UUID,                -- NULL = the seeded walk-in customer
    receipt_header  TEXT,                    -- printed atop receipts
    receipt_footer  TEXT,
    allow_negative_stock_sales BOOLEAN NOT NULL DEFAULT FALSE,  -- offline overshoot policy at close
    grid_layout     JSONB,
    is_active       BOOLEAN NOT NULL DEFAULT TRUE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by      UUID,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by      UUID
);
CREATE UNIQUE INDEX ux_pos_registers_code ON pos_registers (code);
CREATE INDEX ix_pos_registers_warehouse ON pos_registers (warehouse_id);
