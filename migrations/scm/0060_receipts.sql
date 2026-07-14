-- Goods receipts against a PO. Posting a receipt writes the stock movement
-- (inventory_moves + ledger) in the same transaction and bumps received_qty
-- on the PO lines. status: draft | posted | reversed.

CREATE TABLE procurement_receipts (
    id              UUID PRIMARY KEY,
    number          TEXT,
    order_id        UUID NOT NULL REFERENCES procurement_orders (id),
    receipt_date    DATE NOT NULL,
    reference       TEXT,                    -- supplier delivery note number
    -- Logistics detail for the paper trail / disputes.
    carrier         TEXT,
    tracking_no     TEXT,
    vehicle_reg     TEXT,
    delivered_by    TEXT,                    -- driver / courier name
    received_by     UUID,                    -- our user who physically received
    -- Exchange-rate override for this receipt; NULL = the PO's rate.
    exchange_rate   NUMERIC(20, 8),
    memo            TEXT,
    status          TEXT NOT NULL DEFAULT 'draft',
    -- The stock movement this receipt produced at post time.
    move_id         UUID,
    reverses_id     UUID REFERENCES procurement_receipts (id),
    reversed_by_id  UUID REFERENCES procurement_receipts (id),
    posted_at       TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by      UUID,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by      UUID
);
CREATE UNIQUE INDEX ux_procurement_receipts_number ON procurement_receipts (number);
CREATE INDEX ix_procurement_receipts_order ON procurement_receipts (order_id);
CREATE INDEX ix_procurement_receipts_status ON procurement_receipts (status, receipt_date);

CREATE TABLE procurement_receipt_lines (
    id             UUID PRIMARY KEY,
    receipt_id     UUID NOT NULL REFERENCES procurement_receipts (id) ON DELETE CASCADE,
    order_line_id  UUID NOT NULL REFERENCES procurement_order_lines (id),
    line_no        INTEGER NOT NULL,
    -- qty = accepted into stock; rejected_qty stays out of the ledger and
    -- off received_qty (the PO balance remains open for a replacement or a
    -- short-close). Both in the order line's UoM.
    qty            NUMERIC(20, 6) NOT NULL,
    rejected_qty   NUMERIC(20, 6) NOT NULL DEFAULT 0,
    reject_reason  TEXT,
    batch_id       UUID,                     -- inventory_batches, soft ref; batch phase enforces
    memo           TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_procurement_receipt_lines_receipt ON procurement_receipt_lines (receipt_id);
CREATE INDEX ix_procurement_receipt_lines_order_line ON procurement_receipt_lines (order_line_id);
