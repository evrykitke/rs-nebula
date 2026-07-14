-- The optional sourcing stage in front of the purchase order: internal
-- purchase requisitions (who needs what by when) and requests for
-- quotation (which supplier offers it best). Neither touches stock or the
-- GL — they end by feeding a draft PO.

-- Requisitions: draft -> submitted -> approved -> converted, with
-- rejected and cancelled exits.
CREATE TABLE procurement_requisitions (
    id             UUID PRIMARY KEY,
    number         TEXT,                     -- allocated at submit (REQ series)
    warehouse_id   UUID NOT NULL REFERENCES inventory_warehouses (id),
    needed_by      DATE,
    memo           TEXT,
    status         TEXT NOT NULL DEFAULT 'draft',
    reject_reason  TEXT,
    order_id       UUID REFERENCES procurement_orders (id),  -- the PO it became
    submitted_at   TIMESTAMPTZ,
    submitted_by   UUID,
    approved_at    TIMESTAMPTZ,
    approved_by    UUID,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by     UUID,
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by     UUID
);
CREATE UNIQUE INDEX ux_procurement_requisitions_number ON procurement_requisitions (number);
CREATE INDEX ix_procurement_requisitions_status ON procurement_requisitions (status, created_at);

CREATE TABLE procurement_requisition_lines (
    id             UUID PRIMARY KEY,
    requisition_id UUID NOT NULL REFERENCES procurement_requisitions (id) ON DELETE CASCADE,
    line_no        INTEGER NOT NULL,
    item_id        UUID NOT NULL REFERENCES inventory_items (id),
    qty            NUMERIC(20, 6) NOT NULL,
    needed_by      DATE,
    memo           TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_procurement_requisition_lines_req ON procurement_requisition_lines (requisition_id);
CREATE INDEX ix_procurement_requisition_lines_item ON procurement_requisition_lines (item_id);

-- RFQs: draft -> sent -> closed -> awarded, cancellable until awarded.
CREATE TABLE procurement_rfqs (
    id                  UUID PRIMARY KEY,
    number              TEXT,                 -- allocated at send (RFQ series)
    title               TEXT NOT NULL,
    due_date            DATE,
    memo                TEXT,
    status              TEXT NOT NULL DEFAULT 'draft',
    requisition_id      UUID REFERENCES procurement_requisitions (id),
    awarded_supplier_id UUID REFERENCES procurement_suppliers (id),
    order_id            UUID REFERENCES procurement_orders (id),  -- the awarded PO
    sent_at             TIMESTAMPTZ,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by          UUID,
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by          UUID
);
CREATE UNIQUE INDEX ux_procurement_rfqs_number ON procurement_rfqs (number);
CREATE INDEX ix_procurement_rfqs_status ON procurement_rfqs (status, created_at);

CREATE TABLE procurement_rfq_lines (
    id         UUID PRIMARY KEY,
    rfq_id     UUID NOT NULL REFERENCES procurement_rfqs (id) ON DELETE CASCADE,
    line_no    INTEGER NOT NULL,
    item_id    UUID NOT NULL REFERENCES inventory_items (id),
    qty        NUMERIC(20, 6) NOT NULL,
    memo       TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_procurement_rfq_lines_rfq ON procurement_rfq_lines (rfq_id);
CREATE INDEX ix_procurement_rfq_lines_item ON procurement_rfq_lines (item_id);

-- Which suppliers were asked.
CREATE TABLE procurement_rfq_suppliers (
    rfq_id      UUID NOT NULL REFERENCES procurement_rfqs (id) ON DELETE CASCADE,
    supplier_id UUID NOT NULL REFERENCES procurement_suppliers (id),
    invited_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (rfq_id, supplier_id)
);
CREATE INDEX ix_procurement_rfq_suppliers_supplier ON procurement_rfq_suppliers (supplier_id);

-- One quote per supplier per line; re-recording replaces it.
CREATE TABLE procurement_rfq_quotes (
    id             UUID PRIMARY KEY,
    rfq_id         UUID NOT NULL REFERENCES procurement_rfqs (id) ON DELETE CASCADE,
    rfq_line_id    UUID NOT NULL REFERENCES procurement_rfq_lines (id) ON DELETE CASCADE,
    supplier_id    UUID NOT NULL REFERENCES procurement_suppliers (id),
    unit_price     NUMERIC(20, 6) NOT NULL,
    lead_time_days INTEGER,
    min_qty        NUMERIC(20, 6),
    notes          TEXT,
    quoted_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX ux_procurement_rfq_quotes_line_supplier
    ON procurement_rfq_quotes (rfq_line_id, supplier_id);
CREATE INDEX ix_procurement_rfq_quotes_rfq ON procurement_rfq_quotes (rfq_id);
