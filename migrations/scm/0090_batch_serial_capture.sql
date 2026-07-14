-- Batch/serial capture on document lines. Draft lines record the *names*
-- (a lot number, a JSON array of serial numbers); the masters
-- (inventory_batches / inventory_serials, created dormant in 0030) are
-- only written when the document posts, so deleted drafts leave nothing
-- behind. batch_id on the same tables remains the posted resolution.
ALTER TABLE inventory_move_lines
    ADD COLUMN batch_no TEXT,
    ADD COLUMN serial_nos JSONB;
ALTER TABLE procurement_receipt_lines
    ADD COLUMN batch_no TEXT,
    ADD COLUMN serial_nos JSONB;
