-- Chart of accounts: the financial buckets a tenant books against.
-- One row per account, optionally nested (parent_id) into a hierarchy.
-- account_type is one of asset|liability|equity|revenue|expense and fixes
-- the account's normal balance side. currency is an ISO 4217 code from the
-- deployment currency table; a single account never mixes currencies.

CREATE TABLE accounting_accounts (
    id            UUID PRIMARY KEY,
    code          TEXT NOT NULL,
    name          TEXT NOT NULL,
    account_type  TEXT NOT NULL,
    currency      TEXT NOT NULL,
    parent_id     UUID REFERENCES accounting_accounts (id),
    description   TEXT,
    -- A stable role identifier for the accounts the platform seeds and
    -- other modules resolve by (e.g. 'ar', 'vat_output', 'sales'). NULL for
    -- user-created accounts. This is what lets a POS or sales module post
    -- to "the receivables account" without any tenant configuration, while
    -- still letting a tenant remap the role to a different account later.
    system_key    TEXT,
    is_system     BOOLEAN NOT NULL DEFAULT FALSE,
    is_active     BOOLEAN NOT NULL DEFAULT TRUE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Account codes identify an account to humans and must be unique per tenant.
CREATE UNIQUE INDEX ux_accounting_accounts_code ON accounting_accounts (code);
-- Each platform role is fulfilled by at most one account.
CREATE UNIQUE INDEX ux_accounting_accounts_system_key ON accounting_accounts (system_key);
-- The tree is walked from parent to children when building the chart.
CREATE INDEX ix_accounting_accounts_parent ON accounting_accounts (parent_id);
-- The trial balance and statements group and filter by type.
CREATE INDEX ix_accounting_accounts_type ON accounting_accounts (account_type);
