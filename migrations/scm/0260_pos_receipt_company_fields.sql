-- Which parts of the company profile the receipt header shows. All on by
-- default — the shipped behaviour — so a tenant unchecks what it doesn't
-- want on paper (say, a shop that finds tax IDs clutter a till receipt).

ALTER TABLE pos_settings
    ADD COLUMN receipt_show_company_name BOOLEAN NOT NULL DEFAULT TRUE,
    ADD COLUMN receipt_show_address      BOOLEAN NOT NULL DEFAULT TRUE,
    ADD COLUMN receipt_show_contacts     BOOLEAN NOT NULL DEFAULT TRUE,
    ADD COLUMN receipt_show_tax_ids      BOOLEAN NOT NULL DEFAULT TRUE;
