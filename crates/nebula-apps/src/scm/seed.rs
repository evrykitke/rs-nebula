//! Default reference data so a tenant can move stock with zero setup: a
//! "Main" warehouse and a starter set of units of measure. Items and
//! suppliers are real master data the tenant creates itself.
//!
//! Seeding is idempotent per row, keyed on the natural code: a row is
//! inserted only when its code is missing, so the boot rollout can run on
//! every start and defaults added in a later release still reach existing
//! tenants.

use crate::scm::inventory::item::uom;
use crate::scm::inventory::warehouse;
use nebula::error::Result;
use nebula::sea_orm;
use sea_orm::entity::prelude::*;
use sea_orm::{DatabaseConnection, Set, TransactionTrait};
use uuid::Uuid;

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

/// Seed the default warehouse and starter UoMs into `db`. Additive and
/// idempotent; returns `true` if anything was inserted.
pub async fn seed_defaults(db: &DatabaseConnection) -> Result<bool> {
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

    txn.commit().await?;
    Ok(seeded_any)
}
