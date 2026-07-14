-- Item master data: categories, units of measure, items and their extra
-- barcodes. Masters are deliberately wide from day one — every column a
-- mature ERP keeps is present, nullable where the owning feature phase has
-- not landed; storage ships now, enforcement ships with the feature.
-- Files in this module are prefixed with a global apply-order number
-- (lexical order is apply order) so foreign keys always point backwards.

-- Categories are hierarchical and carry the accounting-role and control
-- defaults an item inherits when its own columns are NULL — resolution
-- order: item -> category (walking up the tree) -> tenant default.

CREATE TABLE inventory_categories (
    id          UUID PRIMARY KEY,
    code        TEXT,
    name        TEXT NOT NULL,
    description TEXT,
    parent_id   UUID REFERENCES inventory_categories (id),
    -- Inheritable defaults (all optional):
    default_costing_method   TEXT,            -- moving_average | fifo | standard
    default_uom_id           UUID,
    -- GL role overrides, resolved by the accounting subscriber (GL phase);
    -- role keys, not account ids.
    inventory_account_role   TEXT,            -- default 'inventory.asset'
    cogs_account_role        TEXT,            -- default 'inventory.cogs'
    adjustment_account_role  TEXT,            -- default 'inventory.adjustment'
    is_active   BOOLEAN NOT NULL DEFAULT TRUE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by  UUID,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by  UUID
);
CREATE UNIQUE INDEX ux_inventory_categories_name ON inventory_categories (name);
CREATE UNIQUE INDEX ux_inventory_categories_code ON inventory_categories (code)
    WHERE code IS NOT NULL;
CREATE INDEX ix_inventory_categories_parent ON inventory_categories (parent_id);

CREATE TABLE inventory_uoms (
    id          UUID PRIMARY KEY,
    code        TEXT NOT NULL,          -- 'kg', 'unit', 'box'
    name        TEXT NOT NULL,
    symbol      TEXT,                   -- for document printing
    -- Whether quantities in this UoM may carry decimals (kg yes, unit no).
    fractional  BOOLEAN NOT NULL DEFAULT FALSE,
    is_active   BOOLEAN NOT NULL DEFAULT TRUE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX ux_inventory_uoms_code ON inventory_uoms (code);

-- Generic conversions between UoMs of the same family (1 box = 12 unit).
-- Item-specific purchase-pack factors live on the item itself.
CREATE TABLE inventory_uom_conversions (
    id           UUID PRIMARY KEY,
    from_uom_id  UUID NOT NULL REFERENCES inventory_uoms (id),
    to_uom_id    UUID NOT NULL REFERENCES inventory_uoms (id),
    factor       NUMERIC(20, 8) NOT NULL,   -- qty_to = qty_from * factor
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX ux_inventory_uom_conversions
    ON inventory_uom_conversions (from_uom_id, to_uom_id);

-- Item master. An item is anything the business stocks, consumes, buys or
-- sells. item_type: stockable (tracked in the ledger) | consumable
-- (expensed on receipt, no on-hand tracking) | service (no stock at all).
-- costing_method: moving_average | fifo | standard — only moving_average
-- is accepted until the FIFO/standard phases land.
-- Soft references (preferred_supplier_id, tax code ids,
-- default_warehouse_id) carry no FK: they cross a submodule/app boundary
-- or point at a table migrated later; the service layer validates them.

CREATE TABLE inventory_items (
    id              UUID PRIMARY KEY,

    -- Identity
    sku             TEXT NOT NULL,
    name            TEXT NOT NULL,
    description     TEXT,
    category_id     UUID REFERENCES inventory_categories (id),
    brand           TEXT,
    manufacturer    TEXT,
    manufacturer_part_no TEXT,
    model           TEXT,
    barcode         TEXT,                    -- primary; extra codes in the barcode table
    image_file_id   UUID,                    -- nebula storage StoredFile
    country_of_origin TEXT,                  -- ISO 3166-1 alpha-2
    hs_code         TEXT,                    -- customs tariff code
    notes           TEXT,

    -- Classification & roles
    item_type       TEXT NOT NULL DEFAULT 'stockable',
    is_purchasable  BOOLEAN NOT NULL DEFAULT TRUE,
    is_sellable     BOOLEAN NOT NULL DEFAULT TRUE,
    is_active       BOOLEAN NOT NULL DEFAULT TRUE,

    -- Units
    uom_id          UUID NOT NULL REFERENCES inventory_uoms (id),   -- stock UoM
    purchase_uom_id UUID REFERENCES inventory_uoms (id),            -- NULL = stock UoM
    sales_uom_id    UUID REFERENCES inventory_uoms (id),            -- NULL = stock UoM
    -- Item-specific pack size when the generic conversion table doesn't
    -- apply: 1 purchase UoM = this many stock UoM.
    purchase_uom_factor NUMERIC(20, 8),

    -- Costing & pricing
    costing_method  TEXT NOT NULL DEFAULT 'moving_average',
    standard_cost   NUMERIC(20, 6),          -- used when costing_method = standard
    purchase_price  NUMERIC(20, 4),          -- default PO-line prefill
    last_purchase_price NUMERIC(20, 4),      -- maintained by receipt postings
    selling_price   NUMERIC(20, 4),          -- default sales price (future sales/POS)
    min_selling_price NUMERIC(20, 4),        -- floor for discount control
    purchase_tax_code_id UUID,               -- accounting_tax_codes, soft reference
    sales_tax_code_id    UUID,               -- accounting_tax_codes, soft reference

    -- Procurement planning
    preferred_supplier_id UUID,              -- procurement_suppliers, soft reference
    lead_time_days  INTEGER,
    min_order_qty   NUMERIC(20, 6),
    order_multiple  NUMERIC(20, 6),          -- order in multiples of this
    -- Item-level reorder defaults; per-warehouse overrides live on
    -- inventory_stock_levels and win when set.
    reorder_level   NUMERIC(20, 6),
    reorder_qty     NUMERIC(20, 6),
    max_level       NUMERIC(20, 6),
    safety_stock    NUMERIC(20, 6),

    -- Stock control (stored now, enforced by their feature phases)
    default_warehouse_id UUID,               -- inventory_warehouses, soft reference
    track_batches   BOOLEAN NOT NULL DEFAULT FALSE,
    track_serials   BOOLEAN NOT NULL DEFAULT FALSE,
    shelf_life_days INTEGER,                 -- batch expiry = receipt + shelf life
    warranty_days   INTEGER,
    allow_negative  BOOLEAN NOT NULL DEFAULT FALSE,  -- per-item escape hatch

    -- Physical attributes
    weight          NUMERIC(20, 6),
    weight_uom_id   UUID REFERENCES inventory_uoms (id),
    volume          NUMERIC(20, 6),
    length_mm       NUMERIC(20, 2),
    width_mm        NUMERIC(20, 2),
    height_mm       NUMERIC(20, 2),

    -- GL role overrides (NULL = inherit category, then tenant default)
    inventory_account_role  TEXT,
    cogs_account_role       TEXT,
    adjustment_account_role TEXT,
    expense_account_role    TEXT,            -- consumables: what receipt expenses to

    -- Audit
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by      UUID,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by      UUID
);
CREATE UNIQUE INDEX ux_inventory_items_sku ON inventory_items (sku);
CREATE INDEX ix_inventory_items_category ON inventory_items (category_id);
CREATE INDEX ix_inventory_items_name ON inventory_items (name);
CREATE INDEX ix_inventory_items_active ON inventory_items (is_active);
CREATE INDEX ix_inventory_items_supplier ON inventory_items (preferred_supplier_id)
    WHERE preferred_supplier_id IS NOT NULL;
CREATE UNIQUE INDEX ux_inventory_items_barcode ON inventory_items (barcode)
    WHERE barcode IS NOT NULL;

-- Additional scan codes (case barcodes, legacy codes, supplier labels).
CREATE TABLE inventory_item_barcodes (
    id          UUID PRIMARY KEY,
    item_id     UUID NOT NULL REFERENCES inventory_items (id) ON DELETE CASCADE,
    barcode     TEXT NOT NULL,
    -- The pack this code scans as (a case barcode maps to the case UoM).
    uom_id      UUID REFERENCES inventory_uoms (id),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX ux_inventory_item_barcodes ON inventory_item_barcodes (barcode);
CREATE INDEX ix_inventory_item_barcodes_item ON inventory_item_barcodes (item_id);
