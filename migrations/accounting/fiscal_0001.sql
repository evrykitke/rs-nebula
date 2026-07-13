-- Fiscal years and their monthly periods: the posting calendar.
-- An entry may only be posted into an open period; closing finalises a
-- month and locking makes that permanent.

CREATE TABLE accounting_fiscal_years (
    id         UUID PRIMARY KEY,
    name       TEXT NOT NULL,
    start_date DATE NOT NULL,
    end_date   DATE NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX ux_accounting_fiscal_years_name ON accounting_fiscal_years (name);
CREATE INDEX ix_accounting_fiscal_years_start ON accounting_fiscal_years (start_date);

CREATE TABLE accounting_fiscal_periods (
    id             UUID PRIMARY KEY,
    fiscal_year_id UUID NOT NULL REFERENCES accounting_fiscal_years (id) ON DELETE CASCADE,
    period_number  INT NOT NULL,
    name           TEXT NOT NULL,
    start_date     DATE NOT NULL,
    end_date       DATE NOT NULL,
    -- open | closed | locked
    status         TEXT NOT NULL DEFAULT 'open',
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX ux_accounting_fiscal_periods_year_num
    ON accounting_fiscal_periods (fiscal_year_id, period_number);
CREATE INDEX ix_accounting_fiscal_periods_dates
    ON accounting_fiscal_periods (start_date, end_date);
