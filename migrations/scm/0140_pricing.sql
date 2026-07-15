-- Price lists: a structured answer to "what does this item cost this
-- customer today?". scope: default (all customers) | group | customer —
-- the resolution chain picks the most specific active list whose dates
-- cover the document date, falling through to the item's own selling
-- price. status lifecycle: draft -> active -> archived; only active
-- lists price documents. Promotional lists win ties within a scope.
-- Soft references (item_id -> inventory_items, uom_id -> inventory_uoms)
-- carry no FK across the submodule boundary; the service layer validates.

CREATE TABLE sales_price_lists (
    id              UUID PRIMARY KEY,
    name            TEXT NOT NULL,
    description     TEXT,
    currency        TEXT NOT NULL,           -- ISO 4217; only documents in it match
    scope           TEXT NOT NULL DEFAULT 'default',
    tax_inclusive   BOOLEAN NOT NULL DEFAULT FALSE,   -- retail lists: TRUE
    valid_from      DATE,                    -- NULL = since forever
    valid_to        DATE,                    -- NULL = until archived
    status          TEXT NOT NULL DEFAULT 'draft',
    is_promotional  BOOLEAN NOT NULL DEFAULT FALSE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by      UUID,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by      UUID
);
CREATE UNIQUE INDEX ux_sales_price_lists_name ON sales_price_lists (name);
CREATE INDEX ix_sales_price_lists_status ON sales_price_lists (status);

-- A price for an item on a list. min_qty gives quantity breaks: within a
-- list the line with the highest min_qty <= the ordered quantity wins.
-- Exactly one of unit_price / discount_pct prices the line: a fixed
-- price, or a percentage off the item's default selling price.
CREATE TABLE sales_price_list_items (
    id              UUID PRIMARY KEY,
    price_list_id   UUID NOT NULL REFERENCES sales_price_lists (id) ON DELETE CASCADE,
    item_id         UUID NOT NULL,
    uom_id          UUID,                    -- NULL = the item's stock UoM
    min_qty         NUMERIC(20, 6) NOT NULL DEFAULT 0,
    unit_price      NUMERIC(20, 6),
    discount_pct    NUMERIC(7, 4),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX ux_sales_price_list_items ON sales_price_list_items
    (price_list_id, item_id,
     COALESCE(uom_id, '00000000-0000-0000-0000-000000000000'::uuid), min_qty);
CREATE INDEX ix_sales_price_list_items_item ON sales_price_list_items (item_id);
