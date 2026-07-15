-- Credit notes against a posted sales invoice: a customer credit for
-- returns, pricing errors, damage or goodwill. Posting cannot credit more
-- than was billed (per line, under the invoice row lock) and books the
-- mirror of the invoice entry — Dr Sales + Dr VAT output / Cr AR. Lines
-- flagged restock bring goods back into stock at the *original issue cost*
-- (looked up from the delivery movement's ledger), riding a receipt-type
-- movement whose own value books Dr Inventory / Cr COGS. status: draft |
-- posted | cancelled. Soft references carry no FK across module
-- boundaries; the service validates.

CREATE TABLE sales_credit_notes (
    id              UUID PRIMARY KEY,
    number          TEXT,                    -- allocated at post
    customer_id     UUID NOT NULL REFERENCES sales_customers (id),
    invoice_id      UUID NOT NULL REFERENCES sales_invoices (id),
    credit_date     DATE NOT NULL,
    reason          TEXT NOT NULL,           -- returns | pricing error | damaged | goodwill…
    currency        TEXT NOT NULL,
    exchange_rate   NUMERIC(20, 8) NOT NULL DEFAULT 1,
    tax_inclusive   BOOLEAN NOT NULL DEFAULT FALSE,
    memo            TEXT,
    status          TEXT NOT NULL DEFAULT 'draft',
    move_id         UUID,                    -- restock movement, when any line restocks
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
CREATE UNIQUE INDEX ux_sales_credit_notes_number ON sales_credit_notes (number);
CREATE INDEX ix_sales_credit_notes_customer ON sales_credit_notes (customer_id);
CREATE INDEX ix_sales_credit_notes_invoice ON sales_credit_notes (invoice_id);

CREATE TABLE sales_credit_note_lines (
    id              UUID PRIMARY KEY,
    credit_note_id  UUID NOT NULL REFERENCES sales_credit_notes (id) ON DELETE CASCADE,
    invoice_line_id UUID REFERENCES sales_invoice_lines (id),
    line_no         INTEGER NOT NULL,
    description     TEXT NOT NULL,
    qty             NUMERIC(20, 6) NOT NULL,
    unit_price      NUMERIC(20, 6) NOT NULL,  -- credited price (usually the invoiced one)
    discount_pct    NUMERIC(7, 4),
    tax_code_id     UUID,
    -- Restock: goods physically return and become saleable again. FALSE
    -- for damaged/scrap and service credits.
    restock         BOOLEAN NOT NULL DEFAULT FALSE,
    restock_warehouse_id UUID,               -- required when restock
    batch_no        TEXT,
    batch_id        UUID,
    serial_nos      JSONB,
    memo            TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_sales_credit_note_lines_note ON sales_credit_note_lines (credit_note_id);
CREATE INDEX ix_sales_credit_note_lines_invoice_line ON sales_credit_note_lines (invoice_line_id);
