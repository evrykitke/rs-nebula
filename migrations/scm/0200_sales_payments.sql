-- Customer payments (receipts): the accounts-receivable settlement that
-- closes the order-to-cash cycle. A payment is received from one customer
-- and allocated across that customer's posted invoices (and, with the
-- direction flipped, open credit notes reduce the balance too). The
-- amount equals the sum of its allocations; any unallocated remainder
-- stands as customer credit. Posting books Dr Bank|Cash / Cr AR; an
-- invoice's paid amount and settlement status are derived from posted
-- allocations, never stored, so a reversal restores the position with no
-- write-back. status: draft | posted | reversed. Soft references carry no
-- FK across module boundaries; the service validates.

CREATE TABLE sales_payments (
    id              UUID PRIMARY KEY,
    number          TEXT,                    -- allocated at post
    customer_id     UUID NOT NULL REFERENCES sales_customers (id),
    payment_date    DATE NOT NULL,
    method          TEXT NOT NULL,           -- bank_transfer | cash | mobile_money | cheque | card
    reference       TEXT,
    currency        TEXT NOT NULL,
    exchange_rate   NUMERIC(20, 8) NOT NULL DEFAULT 1,
    amount          NUMERIC(20, 4) NOT NULL,
    memo            TEXT,
    status          TEXT NOT NULL DEFAULT 'draft',
    posted_at       TIMESTAMPTZ,
    posted_by       UUID,
    reversed_at     TIMESTAMPTZ,
    reversed_by     UUID,
    reverse_reason  TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by      UUID,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by      UUID
);
CREATE UNIQUE INDEX ux_sales_payments_number ON sales_payments (number);
CREATE INDEX ix_sales_payments_customer ON sales_payments (customer_id);
CREATE INDEX ix_sales_payments_status ON sales_payments (status, payment_date);

-- One allocation of a payment against a posted invoice or an open credit
-- note: exactly one of invoice_id / credit_note_id is set.
CREATE TABLE sales_payment_allocations (
    id              UUID PRIMARY KEY,
    payment_id      UUID NOT NULL REFERENCES sales_payments (id) ON DELETE CASCADE,
    invoice_id      UUID REFERENCES sales_invoices (id),
    credit_note_id  UUID REFERENCES sales_credit_notes (id),
    amount          NUMERIC(20, 4) NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_sales_payment_allocations_payment ON sales_payment_allocations (payment_id);
CREATE INDEX ix_sales_payment_allocations_invoice ON sales_payment_allocations (invoice_id)
    WHERE invoice_id IS NOT NULL;
