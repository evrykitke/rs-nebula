-- Receipt paper and tender policy on the POS settings singleton.
--
-- require_mpesa_reference: whether an M-Pesa tender must carry its
-- confirmation code at capture. Defaults TRUE — the shipped behaviour —
-- so relaxing it for speed is a deliberate, audited choice.
--
-- The receipt_* columns describe the paper the tills print to (thermal
-- rolls are commonly 58mm or 80mm wide). The client renders the receipt
-- to these dimensions; the server only stores them.

ALTER TABLE pos_settings
    ADD COLUMN require_mpesa_reference BOOLEAN NOT NULL DEFAULT TRUE,
    ADD COLUMN receipt_paper_width_mm  INTEGER NOT NULL DEFAULT 80,
    ADD COLUMN receipt_margin_mm       INTEGER NOT NULL DEFAULT 4,
    ADD COLUMN receipt_font_size_px    INTEGER NOT NULL DEFAULT 12;
