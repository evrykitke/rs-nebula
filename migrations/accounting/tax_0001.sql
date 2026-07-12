-- Tax codes: named rates applied to a transaction's taxable base, each
-- pointing at the account the tax is booked to (VAT output on sales, VAT
-- input on purchases). Rates are percentages (16.0000 = 16%). Seeded with
-- sensible, editable defaults so a business can transact without setting
-- tax up, while a business that cares keeps full control.

CREATE TABLE accounting_tax_codes (
    id              UUID PRIMARY KEY,
    code            TEXT NOT NULL,
    name            TEXT NOT NULL,
    -- Percentage rate, e.g. 16.0000 for 16%.
    rate            NUMERIC(9, 4) NOT NULL DEFAULT 0,
    -- The account the tax posts to; NULL for exempt/no-tax codes.
    account_id      UUID REFERENCES accounting_accounts (id),
    -- Sales tax is collected (a liability); purchase tax is recoverable
    -- (an asset). 'output' = collected on sales, 'input' = paid on
    -- purchases.
    direction       TEXT NOT NULL DEFAULT 'output',
    is_system       BOOLEAN NOT NULL DEFAULT FALSE,
    is_active       BOOLEAN NOT NULL DEFAULT TRUE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX ux_accounting_tax_codes_code ON accounting_tax_codes (code);
CREATE INDEX ix_accounting_tax_codes_account ON accounting_tax_codes (account_id);
