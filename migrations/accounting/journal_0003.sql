-- The GL port books each source document exactly once, keyed by the
-- entry's reference ("{module}.{document}:{id}:{action}"). The duplicate
-- check runs on every delivery, so it must not scan the journal.
CREATE INDEX ix_accounting_journal_reference
    ON accounting_journal_entries (reference)
    WHERE reference IS NOT NULL;
