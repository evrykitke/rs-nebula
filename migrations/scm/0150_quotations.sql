-- Quotations: an offer to a customer — no reservations, no credit
-- effects, no stock. Lifecycle: draft -> sent (numbered) -> accepted |
-- declined | expired, and accepted -> converted once a sales order is
-- cut from it (converted_to_id points at the order; the order's
-- quotation_id points back). Soft references (item_id, uom_id,
-- tax_code_id, price_list_id, salesperson_id) carry no FK across
-- module/file boundaries; the service layer validates.

CREATE TABLE sales_quotations (
    id              UUID PRIMARY KEY,
    number          TEXT,                    -- allocated at send
    customer_id     UUID NOT NULL REFERENCES sales_customers (id),
    quote_date      DATE NOT NULL,
    valid_until     DATE,                    -- NULL = no expiry
    currency        TEXT NOT NULL,
    price_list_id   UUID,                    -- what priced it, for the trail
    tax_inclusive   BOOLEAN NOT NULL DEFAULT FALSE,
    discount_pct    NUMERIC(7, 4),
    discount_amount NUMERIC(20, 4),
    other_charges   NUMERIC(20, 4),
    customer_contact TEXT,
    salesperson_id  UUID,
    memo            TEXT,
    reference       TEXT,                    -- their enquiry/RFQ number
    terms_and_conditions TEXT,
    status          TEXT NOT NULL DEFAULT 'draft',
    sent_at         TIMESTAMPTZ,
    sent_by         UUID,
    resolved_at     TIMESTAMPTZ,             -- accepted/declined/expired moment
    decline_reason  TEXT,
    converted_to_id UUID,                    -- sales_orders, soft ref (created in 0160)
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by      UUID,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by      UUID
);
CREATE UNIQUE INDEX ux_sales_quotations_number ON sales_quotations (number);
CREATE INDEX ix_sales_quotations_customer ON sales_quotations (customer_id);
CREATE INDEX ix_sales_quotations_status_date ON sales_quotations (status, quote_date);

CREATE TABLE sales_quotation_lines (
    id              UUID PRIMARY KEY,
    quotation_id    UUID NOT NULL REFERENCES sales_quotations (id) ON DELETE CASCADE,
    line_no         INTEGER NOT NULL,
    item_id         UUID NOT NULL,
    description     TEXT,
    qty             NUMERIC(20, 6) NOT NULL,
    uom_id          UUID,                    -- NULL = the item's stock UoM
    unit_price      NUMERIC(20, 6) NOT NULL,
    -- Where the price came from: list:{uuid} | item_default | manual.
    price_source    TEXT,
    discount_pct    NUMERIC(7, 4),
    tax_code_id     UUID,
    memo            TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_sales_quotation_lines_quotation ON sales_quotation_lines (quotation_id);
CREATE INDEX ix_sales_quotation_lines_item ON sales_quotation_lines (item_id);
