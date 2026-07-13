-- Defense-in-depth for the journal invariants the service enforces:
-- posting lines are non-negative and single-sided, and a posted entry is
-- reversed at most once (backstops the row-locked reverse operation).

ALTER TABLE accounting_postings
    ADD CONSTRAINT ck_accounting_postings_non_negative CHECK (debit >= 0 AND credit >= 0);
ALTER TABLE accounting_postings
    ADD CONSTRAINT ck_accounting_postings_single_sided CHECK (debit = 0 OR credit = 0);

-- At most one reversal entry may point back at a given original.
CREATE UNIQUE INDEX ux_accounting_journal_reverses
    ON accounting_journal_entries (reverses_id)
    WHERE reverses_id IS NOT NULL;
