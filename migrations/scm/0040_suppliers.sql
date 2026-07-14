-- Suppliers and the item-supplier catalog. supplier_type: company |
-- individual. on_hold blocks NEW purchase orders while existing documents
-- finish their lifecycle — softer than deactivation. Soft references
-- (default_tax_code_id -> accounting_tax_codes, item_id -> inventory_items,
-- purchase_uom_id -> inventory_uoms) deliberately carry no FK: those tables
-- belong to another submodule's schema; the service layer validates.

CREATE TABLE procurement_suppliers (
    id              UUID PRIMARY KEY,

    -- Identity
    code            TEXT NOT NULL,           -- short handle, e.g. 'ACME'
    name            TEXT NOT NULL,           -- trading name
    legal_name      TEXT,                    -- registered name if different
    supplier_type   TEXT NOT NULL DEFAULT 'company',
    registration_no TEXT,                    -- company/business registration
    tax_number      TEXT,                    -- PIN / VAT registration
    industry        TEXT,
    website         TEXT,

    -- Contact
    contact_name    TEXT,
    email           TEXT,
    phone           TEXT,
    secondary_contact_name TEXT,
    secondary_email TEXT,
    secondary_phone TEXT,
    address_line1   TEXT,
    address_line2   TEXT,
    city            TEXT,
    region          TEXT,
    postal_code     TEXT,
    country         TEXT,                    -- ISO 3166-1 alpha-2

    -- Commercial terms
    currency        TEXT NOT NULL,           -- ISO 4217; POs default to this
    payment_terms_days INTEGER NOT NULL DEFAULT 0,   -- 0 = cash/immediate
    credit_limit    NUMERIC(20, 4),          -- our exposure ceiling, base currency
    default_discount_pct NUMERIC(7, 4),      -- negotiated standing discount
    default_tax_code_id UUID,                -- accounting_tax_codes, soft reference
    incoterms       TEXT,                    -- default delivery terms (EXW, FOB, ...)
    lead_time_days  INTEGER,                 -- default; item-supplier rows override
    min_order_value NUMERIC(20, 4),          -- their minimum, supplier currency

    -- Remittance
    bank_name       TEXT,
    bank_branch     TEXT,
    bank_account_name TEXT,
    bank_account_no TEXT,
    bank_swift      TEXT,
    mobile_money_no TEXT,                    -- M-Pesa/paybill style rails
    payment_notes   TEXT,

    -- Status & evaluation
    is_preferred    BOOLEAN NOT NULL DEFAULT FALSE,
    on_hold         BOOLEAN NOT NULL DEFAULT FALSE,
    hold_reason     TEXT,
    rating          NUMERIC(3, 2),           -- 0-5, scorecard phase maintains it
    is_active       BOOLEAN NOT NULL DEFAULT TRUE,
    notes           TEXT,

    -- Audit
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by      UUID,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by      UUID
);
CREATE UNIQUE INDEX ux_procurement_suppliers_code ON procurement_suppliers (code);
CREATE INDEX ix_procurement_suppliers_name ON procurement_suppliers (name);
CREATE INDEX ix_procurement_suppliers_active ON procurement_suppliers (is_active);

-- The item-supplier catalog: who sells us what, under which SKU, at what
-- price lately, how fast. Dormant consumers (price prefill beyond the item
-- default, auto-reorder supplier choice, scorecards) arrive in later phases;
-- receipts maintain last_price/last_purchased_on from day one.
CREATE TABLE procurement_item_suppliers (
    id              UUID PRIMARY KEY,
    item_id         UUID NOT NULL,           -- inventory_items, soft reference
    supplier_id     UUID NOT NULL REFERENCES procurement_suppliers (id) ON DELETE CASCADE,
    supplier_sku    TEXT,                    -- their code for our item
    supplier_item_name TEXT,                 -- their description
    purchase_uom_id UUID,                    -- inventory_uoms, soft reference
    pack_qty        NUMERIC(20, 6),          -- stock units per their pack
    last_price      NUMERIC(20, 6),          -- supplier currency
    last_purchased_on DATE,
    lead_time_days  INTEGER,
    min_order_qty   NUMERIC(20, 6),
    is_preferred    BOOLEAN NOT NULL DEFAULT FALSE,   -- the source auto-reorder picks
    is_active       BOOLEAN NOT NULL DEFAULT TRUE,
    notes           TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX ux_procurement_item_suppliers ON procurement_item_suppliers (item_id, supplier_id);
CREATE INDEX ix_procurement_item_suppliers_supplier ON procurement_item_suppliers (supplier_id);
