-- Purchase invoices (vendor bills) against a PO. status: draft | posted |
-- cancelled. Posting runs the three-way match and (GL phase) requests the
-- AP entry.

CREATE TABLE procurement_invoices (
    id              UUID PRIMARY KEY,
    number          TEXT,                    -- our internal PINV- number
    supplier_id     UUID NOT NULL REFERENCES procurement_suppliers (id),
    order_id        UUID REFERENCES procurement_orders (id),   -- NULL = direct bill (later)
    supplier_invoice_no TEXT NOT NULL,       -- their document number
    invoice_date    DATE NOT NULL,
    due_date        DATE,                    -- NULL = derive from payment terms
    payment_terms_days INTEGER,              -- snapshot; NULL = supplier default
    currency        TEXT NOT NULL,
    exchange_rate   NUMERIC(20, 8) NOT NULL DEFAULT 1,
    tax_inclusive   BOOLEAN NOT NULL DEFAULT FALSE,
    -- Header adjustments mirroring the PO's.
    discount_pct    NUMERIC(7, 4),
    discount_amount NUMERIC(20, 4),
    other_charges   NUMERIC(20, 4),          -- billed freight/handling not on PO lines
    -- Scanned source document in nebula storage.
    attachment_file_id UUID,
    memo            TEXT,
    status          TEXT NOT NULL DEFAULT 'draft',
    posted_at       TIMESTAMPTZ,
    posted_by       UUID,
    cancelled_at    TIMESTAMPTZ,
    cancelled_by    UUID,
    cancel_reason   TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by      UUID,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by      UUID
);
CREATE UNIQUE INDEX ux_procurement_invoices_number ON procurement_invoices (number);
CREATE INDEX ix_procurement_invoices_supplier ON procurement_invoices (supplier_id);
CREATE INDEX ix_procurement_invoices_order ON procurement_invoices (order_id);
CREATE INDEX ix_procurement_invoices_status ON procurement_invoices (status, invoice_date);
-- One supplier cannot bill the same document twice.
CREATE UNIQUE INDEX ux_procurement_invoices_supplier_doc
    ON procurement_invoices (supplier_id, supplier_invoice_no);

CREATE TABLE procurement_invoice_lines (
    id             UUID PRIMARY KEY,
    invoice_id     UUID NOT NULL REFERENCES procurement_invoices (id) ON DELETE CASCADE,
    order_line_id  UUID REFERENCES procurement_order_lines (id),
    line_no        INTEGER NOT NULL,
    description    TEXT NOT NULL,
    qty            NUMERIC(20, 6) NOT NULL,  -- in the order line's UoM
    unit_price     NUMERIC(20, 6) NOT NULL,
    discount_pct   NUMERIC(7, 4),
    tax_code_id    UUID,
    memo           TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_procurement_invoice_lines_invoice ON procurement_invoice_lines (invoice_id);
CREATE INDEX ix_procurement_invoice_lines_order_line ON procurement_invoice_lines (order_line_id);
