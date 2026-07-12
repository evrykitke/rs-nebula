-- Double-entry journal. A journal entry is the atomic unit of financial
-- activity; its postings are the debit/credit lines. An entry balances
-- when the sum of its debits equals the sum of its credits.
--
-- Lifecycle: an entry is created as 'draft' (editable, no number), then
-- 'posted' (immutable, gap-free number allocated, committed to the
-- ledger). A posted entry is never edited or deleted — a correction is a
-- 'reversed' link to a new mirror entry. status is one of
-- draft|posted|reversed. A single entry never mixes currencies.

CREATE TABLE accounting_journal_entries (
    id              UUID PRIMARY KEY,
    -- Allocated from the accounting.journal series only when the entry is
    -- posted; NULL while it is a draft (Postgres unique indexes treat
    -- NULLs as distinct, so many drafts coexist).
    number          TEXT,
    entry_date      DATE NOT NULL,
    memo            TEXT NOT NULL,
    reference       TEXT,
    currency        TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'draft',
    -- When this entry reverses another (and vice versa), the two point at
    -- each other so the audit trail is navigable in both directions.
    reverses_id     UUID REFERENCES accounting_journal_entries (id),
    reversed_by_id  UUID REFERENCES accounting_journal_entries (id),
    posted_at       TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by      UUID
);

-- Posted document numbers are unique; drafts (NULL) are exempt.
CREATE UNIQUE INDEX ux_accounting_journal_number ON accounting_journal_entries (number);
-- The register filters by status and orders by date.
CREATE INDEX ix_accounting_journal_status_date ON accounting_journal_entries (status, entry_date);

CREATE TABLE accounting_postings (
    id           UUID PRIMARY KEY,
    entry_id     UUID NOT NULL REFERENCES accounting_journal_entries (id) ON DELETE CASCADE,
    account_id   UUID NOT NULL REFERENCES accounting_accounts (id),
    line_no      INTEGER NOT NULL,
    -- Exactly one of debit/credit is non-zero on a line; both are stored
    -- as non-negative exact decimals (never floats).
    debit        NUMERIC(20, 4) NOT NULL DEFAULT 0,
    credit       NUMERIC(20, 4) NOT NULL DEFAULT 0,
    memo         TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Loading an entry pulls its lines.
CREATE INDEX ix_accounting_postings_entry ON accounting_postings (entry_id);
-- The account ledger and trial balance sum postings per account.
CREATE INDEX ix_accounting_postings_account ON accounting_postings (account_id);
