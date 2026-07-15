-- Delivery notes against a sales order: where sold goods leave stock.
-- Posting issues stock through the engine (the movement is stamped with
-- the DN- number, the goods-receipt mirror on the way out), consumes the
-- order line's reservation first and takes the rest from free stock,
-- bumps delivered_qty and recomputes the order status — one transaction.
-- COGS (Dr COGS / Cr Inventory) rides on the issue movement's own ledger
-- value. status: draft | posted | reversed. Reversal mirrors the stock
-- back in at the issue costs, re-reserves what returns to the open order,
-- and is blocked once the delivered quantities have been billed. Soft
-- references carry no FK across module/file boundaries.

CREATE TABLE sales_deliveries (
    id              UUID PRIMARY KEY,
    number          TEXT,                    -- allocated at post (shared with the stock movement)
    order_id        UUID NOT NULL REFERENCES sales_orders (id),
    delivery_date   DATE NOT NULL,
    -- Logistics for the paper trail.
    carrier         TEXT,
    tracking_no     TEXT,
    vehicle_reg     TEXT,
    driver_name     TEXT,
    dispatched_by   UUID,                    -- our user
    received_by_name TEXT,                   -- who signed on their side
    shipping_address TEXT,                   -- snapshot override
    memo            TEXT,
    status          TEXT NOT NULL DEFAULT 'draft',
    move_id         UUID,                    -- the stock movement posted
    reverses_id     UUID REFERENCES sales_deliveries (id),
    reversed_by_id  UUID REFERENCES sales_deliveries (id),
    posted_at       TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by      UUID,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by      UUID
);
CREATE UNIQUE INDEX ux_sales_deliveries_number ON sales_deliveries (number);
CREATE INDEX ix_sales_deliveries_order ON sales_deliveries (order_id);
CREATE INDEX ix_sales_deliveries_status ON sales_deliveries (status, delivery_date);

CREATE TABLE sales_delivery_lines (
    id              UUID PRIMARY KEY,
    delivery_id     UUID NOT NULL REFERENCES sales_deliveries (id) ON DELETE CASCADE,
    order_line_id   UUID NOT NULL REFERENCES sales_order_lines (id),
    line_no         INTEGER NOT NULL,
    qty             NUMERIC(20, 6) NOT NULL,  -- in the order line's UoM
    batch_no        TEXT,                     -- the lot as drafted; resolved to batch_id at post
    batch_id        UUID,                     -- inventory_batches, soft ref
    serial_nos      JSONB,                    -- the serial units as drafted
    memo            TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_sales_delivery_lines_delivery ON sales_delivery_lines (delivery_id);
CREATE INDEX ix_sales_delivery_lines_order_line ON sales_delivery_lines (order_line_id);
