-- Purchase orders. Lifecycle:
-- draft -> submitted -> approved -> partially_received -> received -> closed
--        \-> cancelled (only before any posted receipt/invoice)
-- The number is allocated at submit (the moment it becomes a real
-- commitment). Terms are snapshotted at submit so later supplier edits
-- don't rewrite history. received_qty/billed_qty on the lines are the
-- cumulative fulfilment counters that make partial deliveries, partial
-- billing, the over-receipt guard and the GRNI report all fall out.

CREATE TABLE procurement_orders (
    id              UUID PRIMARY KEY,
    number          TEXT,
    supplier_id     UUID NOT NULL REFERENCES procurement_suppliers (id),
    order_date      DATE NOT NULL,
    expected_date   DATE,
    deliver_to_warehouse_id UUID NOT NULL,   -- inventory_warehouses (soft-checked in code)
    -- Free-text override when delivering somewhere other than the warehouse
    -- address (a site, a customer, a branch under fit-out).
    delivery_address TEXT,
    shipping_method TEXT,
    incoterms       TEXT,                    -- snapshot; defaults from the supplier
    supplier_contact TEXT,                   -- who we're dealing with on their side
    buyer_id        UUID,                    -- our responsible user
    currency        TEXT NOT NULL,           -- supplier currency
    exchange_rate   NUMERIC(20, 8) NOT NULL DEFAULT 1,  -- to tenant base, captured at approval
    payment_terms_days INTEGER NOT NULL DEFAULT 0,
    tax_inclusive   BOOLEAN NOT NULL DEFAULT FALSE,     -- line prices include tax
    -- Header-level discount applied after line discounts.
    discount_pct    NUMERIC(7, 4),
    discount_amount NUMERIC(20, 4),
    other_charges   NUMERIC(20, 4),          -- quoted freight/handling on the PO paper
    memo            TEXT,
    reference       TEXT,                    -- supplier's quote/ref number
    terms_and_conditions TEXT,               -- printed on the PO document
    status          TEXT NOT NULL DEFAULT 'draft',
    submitted_at    TIMESTAMPTZ,
    submitted_by    UUID,
    approved_at     TIMESTAMPTZ,
    approved_by     UUID,
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
CREATE UNIQUE INDEX ux_procurement_orders_number ON procurement_orders (number);
CREATE INDEX ix_procurement_orders_supplier ON procurement_orders (supplier_id);
CREATE INDEX ix_procurement_orders_status_date ON procurement_orders (status, order_date);
CREATE INDEX ix_procurement_orders_buyer ON procurement_orders (buyer_id)
    WHERE buyer_id IS NOT NULL;

CREATE TABLE procurement_order_lines (
    id            UUID PRIMARY KEY,
    order_id      UUID NOT NULL REFERENCES procurement_orders (id) ON DELETE CASCADE,
    line_no       INTEGER NOT NULL,
    item_id       UUID NOT NULL,             -- inventory_items (soft-checked in code)
    description   TEXT,                      -- overrides item name on the paper
    qty           NUMERIC(20, 6) NOT NULL,
    -- UoM the line was ordered in (soft ref; NULL = the item's stock UoM).
    -- received_qty/billed_qty count in this same UoM; conversion to stock
    -- UoM happens where the receipt writes the stock movement.
    uom_id        UUID,
    unit_price    NUMERIC(20, 6) NOT NULL,   -- in order currency, before tax
    discount_pct  NUMERIC(7, 4),             -- line discount off unit_price
    tax_code_id   UUID,
    expected_date DATE,                      -- per-line schedule; NULL = header's
    -- Cumulative fulfilment counters, the heart of partials + 3-way match.
    received_qty  NUMERIC(20, 6) NOT NULL DEFAULT 0,
    billed_qty    NUMERIC(20, 6) NOT NULL DEFAULT 0,
    memo          TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_procurement_order_lines_order ON procurement_order_lines (order_id);
CREATE INDEX ix_procurement_order_lines_item ON procurement_order_lines (item_id);
