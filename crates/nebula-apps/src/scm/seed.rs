//! Default reference data so a tenant can move stock with zero setup: a
//! "Main" warehouse, a starter set of units of measure, and a walk-in
//! customer (cash only) so a counter sale needs no master data first.
//! Items, suppliers and real customers are master data the tenant
//! creates itself.
//!
//! Seeding is idempotent per row, keyed on the natural code: a row is
//! inserted only when its code is missing, so the boot rollout can run on
//! every start and defaults added in a later release still reach existing
//! tenants.

use crate::scm::inventory::item::uom;
use crate::scm::inventory::warehouse;
use crate::scm::sales::customer::customer;
use nebula::error::Result;
use nebula::sea_orm;
use rust_decimal::Decimal;
use sea_orm::entity::prelude::*;
use sea_orm::{DatabaseConnection, Set, TransactionTrait};
use uuid::Uuid;

/// The seeded walk-in customer's code.
pub const WALK_IN_CODE: &str = "WALKIN";

/// code, name, symbol, fractional.
const DEFAULT_UOMS: &[(&str, &str, &str, bool)] = &[
    ("unit", "Unit", "ea", false),
    ("kg", "Kilogram", "kg", true),
    ("g", "Gram", "g", true),
    ("l", "Litre", "L", true),
    ("ml", "Millilitre", "mL", true),
    ("m", "Metre", "m", true),
    ("box", "Box", "box", false),
    ("pack", "Pack", "pk", false),
];

/// Seed the default warehouse, starter UoMs and the walk-in customer
/// (in `currency`, the tenant's own) into `db`. Additive and idempotent;
/// returns `true` if anything was inserted.
pub async fn seed_defaults(db: &DatabaseConnection, currency: &str) -> Result<bool> {
    let now = chrono::Utc::now();
    let mut seeded_any = false;
    let txn = db.begin().await?;

    for (code, name, symbol, fractional) in DEFAULT_UOMS {
        let existing = uom::Entity::find()
            .filter(uom::Column::Code.eq(*code))
            .count(&txn)
            .await?;
        if existing > 0 {
            continue;
        }
        uom::ActiveModel {
            id: Set(Uuid::new_v4()),
            code: Set(String::from(*code)),
            name: Set(String::from(*name)),
            symbol: Set(Some(String::from(*symbol))),
            fractional: Set(*fractional),
            is_active: Set(true),
            created_at: Set(now),
        }
        .insert(&txn)
        .await?;
        seeded_any = true;
    }

    let main = warehouse::Entity::find()
        .filter(warehouse::Column::Code.eq("MAIN"))
        .count(&txn)
        .await?;
    if main == 0 {
        // Default only when the tenant hasn't already chosen its own.
        let has_default = warehouse::Entity::find()
            .filter(warehouse::Column::IsDefault.eq(true))
            .count(&txn)
            .await?;
        warehouse::ActiveModel {
            id: Set(Uuid::new_v4()),
            code: Set("MAIN".to_string()),
            name: Set("Main".to_string()),
            warehouse_type: Set("standard".to_string()),
            parent_id: Set(None),
            address_line1: Set(None),
            address_line2: Set(None),
            city: Set(None),
            region: Set(None),
            postal_code: Set(None),
            country: Set(None),
            phone: Set(None),
            email: Set(None),
            contact_name: Set(None),
            is_default: Set(has_default == 0),
            allow_negative: Set(false),
            is_active: Set(true),
            notes: Set(None),
            created_at: Set(now),
            created_by: Set(None),
            updated_at: Set(now),
            updated_by: Set(None),
        }
        .insert(&txn)
        .await?;
        seeded_any = true;
    }

    // The walk-in customer: a POS/counter sale needs a buyer on the
    // paper, and a brand-new tenant has none. Cash only (credit_limit 0)
    // in the tenant's own currency.
    let walk_in = customer::Entity::find()
        .filter(customer::Column::Code.eq(WALK_IN_CODE))
        .count(&txn)
        .await?;
    if walk_in == 0 {
        customer::ActiveModel {
            id: Set(Uuid::new_v4()),
            code: Set(WALK_IN_CODE.to_string()),
            name: Set("Walk-in customer".to_string()),
            legal_name: Set(None),
            customer_type: Set("individual".to_string()),
            registration_no: Set(None),
            tax_number: Set(None),
            industry: Set(None),
            website: Set(None),
            group_id: Set(None),
            contact_name: Set(None),
            email: Set(None),
            phone: Set(None),
            secondary_contact_name: Set(None),
            secondary_email: Set(None),
            secondary_phone: Set(None),
            billing_address_line1: Set(None),
            billing_address_line2: Set(None),
            billing_city: Set(None),
            billing_region: Set(None),
            billing_postal_code: Set(None),
            billing_country: Set(None),
            shipping_address_line1: Set(None),
            shipping_address_line2: Set(None),
            shipping_city: Set(None),
            shipping_region: Set(None),
            shipping_postal_code: Set(None),
            shipping_country: Set(None),
            currency: Set(currency.to_string()),
            payment_terms_days: Set(0),
            credit_limit: Set(Some(Decimal::ZERO)),
            price_list_id: Set(None),
            default_discount_pct: Set(None),
            default_tax_code_id: Set(None),
            tax_exempt: Set(false),
            tax_exemption_no: Set(None),
            default_warehouse_id: Set(None),
            salesperson_id: Set(None),
            incoterms: Set(None),
            loyalty_no: Set(None),
            on_hold: Set(false),
            hold_reason: Set(None),
            is_active: Set(true),
            notes: Set(Some(
                "Seeded default for counter sales; cash only.".to_string(),
            )),
            created_at: Set(now),
            created_by: Set(None),
            updated_at: Set(now),
            updated_by: Set(None),
        }
        .insert(&txn)
        .await?;
        seeded_any = true;
    }

    txn.commit().await?;
    Ok(seeded_any)
}
