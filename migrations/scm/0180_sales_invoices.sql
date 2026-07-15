-- Sales invoices: the accounts-receivable spine. Posting runs the billing
-- consistency check per line against the sales order under its row lock
-- (bill only what has shipped: billed_qty + qty <= delivered_qty), bumps
-- billed_qty, computes net/tax/gross from the accounting tax codes
-- honouring tax_inclusive, and books Dr AR / Cr Sales / Cr VAT output
-- (rounding residue to the rounding role) through the GL port. Payment
-- state (open / partially_paid / paid) is derived from posted allocations,
-- never stored. status: draft | posted | cancelled. Cancellation restores
-- billed_qty and books the mirror entry. Soft references carry no FK
-- across module boundaries; the service validates.

CREATE TABLE sales_invoices (
    id              UUID PRIMARY KEY,
    number          TEXT,                    -- allocated at post
    customer_id     UUID NOT NULL REFERENCES sales_customers (id),
    order_id        UUID REFERENCES sales_orders (id),   -- NULL = direct invoice (later phase)
    invoice_date    DATE NOT NULL,
    due_date        DATE,                    -- NULL = derive from terms at post
    payment_terms_days INTEGER,
    currency        TEXT NOT NULL,
    exchange_rate   NUMERIC(20, 8) NOT NULL DEFAULT 1,
    tax_inclusive   BOOLEAN NOT NULL DEFAULT FALSE,
    discount_pct    NUMERIC(7, 4),
    discount_amount NUMERIC(20, 4),
    other_charges   NUMERIC(20, 4),
    customer_po_no  TEXT,
    salesperson_id  UUID,
    attachment_file_id UUID,                 -- signed copy / supporting doc
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
CREATE UNIQUE INDEX ux_sales_invoices_number ON sales_invoices (number);
CREATE INDEX ix_sales_invoices_customer ON sales_invoices (customer_id);
CREATE INDEX ix_sales_invoices_order ON sales_invoices (order_id);
CREATE INDEX ix_sales_invoices_status ON sales_invoices (status, invoice_date);
CREATE INDEX ix_sales_invoices_due ON sales_invoices (due_date) WHERE due_date IS NOT NULL;

CREATE TABLE sales_invoice_lines (
    id              UUID PRIMARY KEY,
    invoice_id      UUID NOT NULL REFERENCES sales_invoices (id) ON DELETE CASCADE,
    order_line_id   UUID REFERENCES sales_order_lines (id),
    line_no         INTEGER NOT NULL,
    description     TEXT NOT NULL,
    qty             NUMERIC(20, 6) NOT NULL,
    unit_price      NUMERIC(20, 6) NOT NULL,
    discount_pct    NUMERIC(7, 4),
    tax_code_id     UUID,
    memo            TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_sales_invoice_lines_invoice ON sales_invoice_lines (invoice_id);
CREATE INDEX ix_sales_invoice_lines_order_line ON sales_invoice_lines (order_line_id);
