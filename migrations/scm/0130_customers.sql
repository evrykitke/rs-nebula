-- Customers and customer groups: who we sell to, on what terms.
-- customer_type: company | individual. on_hold blocks NEW sales documents
-- while existing documents finish their lifecycle — softer than
-- deactivation (the supplier precedent). credit_limit semantics: NULL =
-- unlimited credit, 0 = cash only, > 0 = the ceiling checked at order
-- confirmation and invoice posting. Soft references (price_list_id ->
-- sales_price_lists created in 0140, default_tax_code_id ->
-- accounting_tax_codes, default_warehouse_id -> inventory_warehouses,
-- salesperson_id -> the administration user table) deliberately carry no
-- FK: those tables belong to another file's or module's schema; the
-- service layer validates.

CREATE TABLE sales_customer_groups (
    id              UUID PRIMARY KEY,
    name            TEXT NOT NULL,
    description     TEXT,
    price_list_id   UUID,                    -- the group's pricing tier
    default_discount_pct NUMERIC(7, 4),
    is_active       BOOLEAN NOT NULL DEFAULT TRUE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by      UUID,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by      UUID
);
CREATE UNIQUE INDEX ux_sales_customer_groups_name ON sales_customer_groups (name);

CREATE TABLE sales_customers (
    id              UUID PRIMARY KEY,

    -- Identity
    code            TEXT NOT NULL,           -- short handle, e.g. 'ACME'
    name            TEXT NOT NULL,           -- trading name
    legal_name      TEXT,                    -- registered name if different
    customer_type   TEXT NOT NULL DEFAULT 'company',
    registration_no TEXT,                    -- company/business registration
    tax_number      TEXT,                    -- PIN / VAT registration
    industry        TEXT,
    website         TEXT,
    group_id        UUID REFERENCES sales_customer_groups (id),

    -- Contact
    contact_name    TEXT,
    email           TEXT,
    phone           TEXT,
    secondary_contact_name TEXT,
    secondary_email TEXT,
    secondary_phone TEXT,
    billing_address_line1 TEXT,
    billing_address_line2 TEXT,
    billing_city    TEXT,
    billing_region  TEXT,
    billing_postal_code TEXT,
    billing_country TEXT,                    -- ISO 3166-1 alpha-2
    -- NULL shipping fields = same as billing.
    shipping_address_line1 TEXT,
    shipping_address_line2 TEXT,
    shipping_city   TEXT,
    shipping_region TEXT,
    shipping_postal_code TEXT,
    shipping_country TEXT,

    -- Commercial terms
    currency        TEXT NOT NULL,           -- ISO 4217; documents default to this
    payment_terms_days INTEGER NOT NULL DEFAULT 0,   -- 0 = cash/immediate
    credit_limit    NUMERIC(20, 4),          -- NULL = unlimited, 0 = cash only
    price_list_id   UUID,                    -- customer-specific pricing
    default_discount_pct NUMERIC(7, 4),      -- negotiated standing discount
    default_tax_code_id UUID,
    tax_exempt      BOOLEAN NOT NULL DEFAULT FALSE,
    tax_exemption_no TEXT,
    default_warehouse_id UUID,               -- fulfilment source prefill
    salesperson_id  UUID,                    -- our responsible user
    incoterms       TEXT,
    loyalty_no      TEXT,                    -- dormant until a loyalty phase

    -- Status
    on_hold         BOOLEAN NOT NULL DEFAULT FALSE,
    hold_reason     TEXT,
    is_active       BOOLEAN NOT NULL DEFAULT TRUE,
    notes           TEXT,

    -- Audit
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by      UUID,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by      UUID
);
CREATE UNIQUE INDEX ux_sales_customers_code ON sales_customers (code);
CREATE INDEX ix_sales_customers_name ON sales_customers (name);
CREATE INDEX ix_sales_customers_group ON sales_customers (group_id);
CREATE INDEX ix_sales_customers_active ON sales_customers (is_active);
CREATE INDEX ix_sales_customers_salesperson ON sales_customers (salesperson_id)
    WHERE salesperson_id IS NOT NULL;
