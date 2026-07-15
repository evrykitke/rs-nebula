-- Sales orders: the hub of the order-to-cash cycle. Lifecycle:
-- draft -> confirmed (numbered, terms snapshotted, credit checked, stock
-- reserved) -> partially_delivered -> delivered -> closed; cancellation
-- only while nothing posted references the order. Deliveries and
-- invoices maintain the cumulative delivered_qty / billed_qty counters
-- on the lines — the mirror of procurement's received_qty / billed_qty.
-- reserved_qty is what the confirmation (or a later reserve retry)
-- currently holds on the level rows for the line; deliveries consume it,
-- cancellation and close release it. Soft references carry no FK across
-- module/file boundaries; the service layer validates.

CREATE TABLE sales_orders (
    id              UUID PRIMARY KEY,
    number          TEXT,                    -- allocated at confirm
    customer_id     UUID NOT NULL REFERENCES sales_customers (id),
    quotation_id    UUID REFERENCES sales_quotations (id),
    order_date      DATE NOT NULL,
    expected_date   DATE,                    -- promised delivery
    warehouse_id    UUID NOT NULL,           -- fulfilment source; lines may override
    -- Shipping snapshot (the customer master may change later).
    shipping_address TEXT,
    shipping_method TEXT,
    incoterms       TEXT,
    customer_contact TEXT,
    customer_po_no  TEXT,                    -- their purchase order number
    salesperson_id  UUID,
    currency        TEXT NOT NULL,
    exchange_rate   NUMERIC(20, 8) NOT NULL DEFAULT 1,  -- to base, captured at confirm
    price_list_id   UUID,                    -- what priced it, for the trail
    payment_terms_days INTEGER NOT NULL DEFAULT 0,      -- snapshot from the customer
    tax_inclusive   BOOLEAN NOT NULL DEFAULT FALSE,
    discount_pct    NUMERIC(7, 4),
    discount_amount NUMERIC(20, 4),
    other_charges   NUMERIC(20, 4),
    memo            TEXT,
    terms_and_conditions TEXT,
    status          TEXT NOT NULL DEFAULT 'draft',
    confirmed_at    TIMESTAMPTZ,
    confirmed_by    UUID,
    credit_override_by UUID,                 -- who waved it past the credit check
    cancelled_at    TIMESTAMPTZ,
    cancelled_by    UUID,
    cancel_reason   TEXT,
    closed_at       TIMESTAMPTZ,
    closed_by       UUID,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by      UUID,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by      UUID
);
CREATE UNIQUE INDEX ux_sales_orders_number ON sales_orders (number);
CREATE INDEX ix_sales_orders_customer ON sales_orders (customer_id);
CREATE INDEX ix_sales_orders_status_date ON sales_orders (status, order_date);
CREATE INDEX ix_sales_orders_quotation ON sales_orders (quotation_id)
    WHERE quotation_id IS NOT NULL;
CREATE INDEX ix_sales_orders_salesperson ON sales_orders (salesperson_id)
    WHERE salesperson_id IS NOT NULL;

CREATE TABLE sales_order_lines (
    id              UUID PRIMARY KEY,
    order_id        UUID NOT NULL REFERENCES sales_orders (id) ON DELETE CASCADE,
    line_no         INTEGER NOT NULL,
    item_id         UUID NOT NULL,
    description     TEXT,
    qty             NUMERIC(20, 6) NOT NULL,
    uom_id          UUID,                    -- NULL = the item's stock UoM
    warehouse_id    UUID,                    -- NULL = the header's
    unit_price      NUMERIC(20, 6) NOT NULL,
    price_source    TEXT,                    -- list:{uuid} | item_default | manual
    discount_pct    NUMERIC(7, 4),
    tax_code_id     UUID,
    expected_date   DATE,                    -- per-line schedule
    -- Reservation state + fulfilment counters, all maintained under the
    -- order row lock.
    reserved_qty    NUMERIC(20, 6) NOT NULL DEFAULT 0,
    delivered_qty   NUMERIC(20, 6) NOT NULL DEFAULT 0,
    billed_qty      NUMERIC(20, 6) NOT NULL DEFAULT 0,
    memo            TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_sales_order_lines_order ON sales_order_lines (order_id);
CREATE INDEX ix_sales_order_lines_item ON sales_order_lines (item_id);
