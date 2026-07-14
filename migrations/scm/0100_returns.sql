-- Purchase returns (return-to-supplier). A return sends previously
-- received, NOT yet billed goods back: stock leaves through the engine,
-- the order line's received_qty reopens, and the GL relieves GRNI (billed
-- goods must have their invoice cancelled first — the debit-note flow
-- against AP arrives with accounting's payment phase). Lifecycle mirrors
-- the goods receipt: draft -> posted -> reversed.
CREATE TABLE procurement_returns (
    id              UUID PRIMARY KEY,
    number          TEXT,                     -- allocated at post (RTS series)
    order_id        UUID NOT NULL REFERENCES procurement_orders (id),
    return_date     DATE NOT NULL,
    reason          TEXT,                     -- why the goods went back
    reference       TEXT,                     -- supplier RMA / collection note
    carrier         TEXT,
    memo            TEXT,
    status          TEXT NOT NULL DEFAULT 'draft',
    move_id         UUID REFERENCES inventory_moves (id),
    reverses_id     UUID REFERENCES procurement_returns (id),
    reversed_by_id  UUID REFERENCES procurement_returns (id),
    posted_at       TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by      UUID,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by      UUID
);
CREATE UNIQUE INDEX ux_procurement_returns_number ON procurement_returns (number);
CREATE INDEX ix_procurement_returns_order ON procurement_returns (order_id);
CREATE INDEX ix_procurement_returns_status_date ON procurement_returns (status, return_date);

CREATE TABLE procurement_return_lines (
    id            UUID PRIMARY KEY,
    return_id     UUID NOT NULL REFERENCES procurement_returns (id) ON DELETE CASCADE,
    order_line_id UUID NOT NULL REFERENCES procurement_order_lines (id),
    line_no       INTEGER NOT NULL,
    qty           NUMERIC(20, 6) NOT NULL,
    -- Tracking capture, same shape as receipt lines.
    batch_no      TEXT,
    batch_id      UUID REFERENCES inventory_batches (id),
    serial_nos    JSONB,
    reason        TEXT,
    memo          TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_procurement_return_lines_return ON procurement_return_lines (return_id);
CREATE INDEX ix_procurement_return_lines_order_line ON procurement_return_lines (order_line_id);
