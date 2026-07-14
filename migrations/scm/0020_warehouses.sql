-- Warehouses: the physical (or logical) places stock lives, one stock
-- level row per item x warehouse. warehouse_type: standard (normal stock)
-- | transit (goods moving between warehouses, later two-step transfers)
-- | scrap (write-off destination) — only 'standard' is used until the
-- relevant phases. Negative stock needs BOTH item.allow_negative AND
-- warehouse.allow_negative, so the engine's block stands until a tenant
-- deliberately opts a location and item in.

CREATE TABLE inventory_warehouses (
    id             UUID PRIMARY KEY,
    code           TEXT NOT NULL,
    name           TEXT NOT NULL,
    warehouse_type TEXT NOT NULL DEFAULT 'standard',
    parent_id      UUID REFERENCES inventory_warehouses (id),  -- grouping / future bins
    -- Location & contact
    address_line1  TEXT,
    address_line2  TEXT,
    city           TEXT,
    region         TEXT,
    postal_code    TEXT,
    country        TEXT,                    -- ISO 3166-1 alpha-2
    phone          TEXT,
    email          TEXT,
    contact_name   TEXT,
    -- Controls
    is_default     BOOLEAN NOT NULL DEFAULT FALSE,  -- prefilled on documents
    allow_negative BOOLEAN NOT NULL DEFAULT FALSE,  -- per-warehouse escape hatch
    is_active      BOOLEAN NOT NULL DEFAULT TRUE,
    notes          TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by     UUID,
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by     UUID
);
CREATE UNIQUE INDEX ux_inventory_warehouses_code ON inventory_warehouses (code);
CREATE INDEX ix_inventory_warehouses_parent ON inventory_warehouses (parent_id);
-- At most one default warehouse.
CREATE UNIQUE INDEX ux_inventory_warehouses_default
    ON inventory_warehouses (is_default) WHERE is_default;
