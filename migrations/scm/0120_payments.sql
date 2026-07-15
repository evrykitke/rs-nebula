-- Supplier payments: the accounts-payable settlement that closes the
-- purchase-to-pay cycle. A payment belongs to one supplier and is allocated
-- across one or more of that supplier's posted purchase invoices. status:
-- draft | posted | reversed. Posting books Dr Accounts Payable / Cr Bank|Cash
-- through the GL port; an invoice's paid amount and settlement status are
-- derived from the posted allocations, never stored on the invoice.

CREATE TABLE procurement_payments (
    id              UUID PRIMARY KEY,
    number          TEXT,                       -- our internal PAY- number
    supplier_id     UUID NOT NULL REFERENCES procurement_suppliers (id),
    payment_date    DATE NOT NULL,
    method          TEXT NOT NULL DEFAULT 'bank_transfer', -- bank_transfer | cash | mobile_money | cheque | card
    reference       TEXT,                       -- cheque no / transfer / mobile-money ref
    currency        TEXT NOT NULL,
    exchange_rate   NUMERIC(20, 8) NOT NULL DEFAULT 1,
    amount          NUMERIC(20, 4) NOT NULL,    -- total, payment currency
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
CREATE UNIQUE INDEX ux_procurement_payments_number ON procurement_payments (number);
CREATE INDEX ix_procurement_payments_supplier ON procurement_payments (supplier_id);
CREATE INDEX ix_procurement_payments_status ON procurement_payments (status, payment_date);

CREATE TABLE procurement_payment_allocations (
    id             UUID PRIMARY KEY,
    payment_id     UUID NOT NULL REFERENCES procurement_payments (id) ON DELETE CASCADE,
    invoice_id     UUID NOT NULL REFERENCES procurement_invoices (id),
    amount         NUMERIC(20, 4) NOT NULL,     -- allocated to the invoice, payment currency
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_procurement_payment_allocations_payment ON procurement_payment_allocations (payment_id);
CREATE INDEX ix_procurement_payment_allocations_invoice ON procurement_payment_allocations (invoice_id);
