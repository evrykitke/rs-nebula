//! The default chart of accounts and tax codes.
//!
//! Seeded once per tenant database so a business can sell and read reports
//! with zero accounting configuration (think a POS handed to a small
//! retailer). Every seeded account carries a stable [`account::keys`] role
//! so other modules post to it without configuration; a business that
//! cares can rename, deactivate or extend the chart freely.
//!
//! Seeding is idempotent — it does nothing if the chart already has system
//! accounts — so it is safe to run on tenant creation and again on every
//! boot rollout.

use crate::accounting::account::{self, AccountType};
use crate::accounting::tax::{self, TaxDirection};
use nebula::error::Result;
use nebula::sea_orm;
use rust_decimal::Decimal;
use sea_orm::entity::prelude::*;
use sea_orm::{DatabaseConnection, Set, TransactionTrait};
use uuid::Uuid;

/// A postable detail account under a header: code, name and platform role.
struct SeedChild {
    code: &'static str,
    name: &'static str,
    system_key: &'static str,
}

/// A header account grouping its detail accounts. The header is a
/// non-role organizing node; postings are made to its children.
struct SeedGroup {
    code: &'static str,
    name: &'static str,
    account_type: AccountType,
    children: &'static [SeedChild],
}

/// A standard small-business chart of accounts, two levels deep: a header
/// per class (1000/2000/3000/4000/5000/6000) with the postable detail
/// accounts under it.
fn default_accounts() -> Vec<SeedGroup> {
    use AccountType::*;
    use account::keys;
    vec![
        SeedGroup {
            code: "1000",
            name: "Assets",
            account_type: Asset,
            children: &[
                SeedChild {
                    code: "1010",
                    name: "Cash on Hand",
                    system_key: keys::CASH,
                },
                SeedChild {
                    code: "1020",
                    name: "Bank Account",
                    system_key: keys::BANK,
                },
                SeedChild {
                    code: "1100",
                    name: "Accounts Receivable",
                    system_key: keys::AR,
                },
                SeedChild {
                    code: "1200",
                    name: "Inventory",
                    system_key: keys::INVENTORY,
                },
                SeedChild {
                    code: "1300",
                    name: "VAT Receivable (Input)",
                    system_key: keys::VAT_INPUT,
                },
            ],
        },
        SeedGroup {
            code: "2000",
            name: "Liabilities",
            account_type: Liability,
            children: &[
                SeedChild {
                    code: "2100",
                    name: "Accounts Payable",
                    system_key: keys::AP,
                },
                SeedChild {
                    code: "2200",
                    name: "VAT Payable (Output)",
                    system_key: keys::VAT_OUTPUT,
                },
                SeedChild {
                    code: "2300",
                    name: "Tax Payable",
                    system_key: keys::TAX_PAYABLE,
                },
            ],
        },
        SeedGroup {
            code: "3000",
            name: "Equity",
            account_type: Equity,
            children: &[
                SeedChild {
                    code: "3100",
                    name: "Owner's Equity",
                    system_key: keys::OWNER_EQUITY,
                },
                SeedChild {
                    code: "3200",
                    name: "Retained Earnings",
                    system_key: keys::RETAINED_EARNINGS,
                },
            ],
        },
        SeedGroup {
            code: "4000",
            name: "Revenue",
            account_type: Revenue,
            children: &[
                SeedChild {
                    code: "4100",
                    name: "Sales Revenue",
                    system_key: keys::SALES,
                },
                SeedChild {
                    code: "4900",
                    name: "Other Income",
                    system_key: keys::OTHER_INCOME,
                },
            ],
        },
        SeedGroup {
            code: "5000",
            name: "Cost of Sales",
            account_type: Expense,
            children: &[SeedChild {
                code: "5100",
                name: "Cost of Goods Sold",
                system_key: keys::COGS,
            }],
        },
        SeedGroup {
            code: "6000",
            name: "Expenses",
            account_type: Expense,
            children: &[
                SeedChild {
                    code: "6100",
                    name: "Operating Expenses",
                    system_key: keys::OPEX,
                },
                SeedChild {
                    code: "6900",
                    name: "Rounding",
                    system_key: keys::ROUNDING,
                },
            ],
        },
    ]
}

/// Seed the default chart of accounts and tax codes into `db`, in
/// `currency`. Returns `true` if it seeded, `false` if the chart was
/// already set up (idempotent).
pub async fn seed_defaults(db: &DatabaseConnection, currency: &str) -> Result<bool> {
    // Already seeded? Any system account means the chart is in place.
    let existing = account::Entity::find()
        .filter(account::Column::SystemKey.is_not_null())
        .count(db)
        .await?;
    if existing > 0 {
        return Ok(false);
    }

    let now = chrono::Utc::now();
    let txn = db.begin().await?;

    for group in default_accounts() {
        let parent_id = Uuid::new_v4();
        // The header account: a system account with no role, organizing
        // its postable children.
        account::ActiveModel {
            id: Set(parent_id),
            code: Set(group.code.to_string()),
            name: Set(group.name.to_string()),
            account_type: Set(group.account_type.as_str().to_string()),
            currency: Set(currency.to_string()),
            parent_id: Set(None),
            description: Set(None),
            system_key: Set(None),
            is_system: Set(true),
            is_active: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&txn)
        .await?;

        for child in group.children {
            account::ActiveModel {
                id: Set(Uuid::new_v4()),
                code: Set(child.code.to_string()),
                name: Set(child.name.to_string()),
                account_type: Set(group.account_type.as_str().to_string()),
                currency: Set(currency.to_string()),
                parent_id: Set(Some(parent_id)),
                description: Set(None),
                system_key: Set(Some(child.system_key.to_string())),
                is_system: Set(true),
                is_active: Set(true),
                created_at: Set(now),
                updated_at: Set(now),
            }
            .insert(&txn)
            .await?;
        }
    }

    // Resolve the two VAT accounts the default tax codes book to.
    let vat_output = system_account_id(&txn, account::keys::VAT_OUTPUT).await?;
    let vat_input = system_account_id(&txn, account::keys::VAT_INPUT).await?;

    // Editable defaults: a standard sales rate, a zero rate, an exempt
    // code (no tax line), and the matching purchase (input) rate.
    let tax_codes = [
        (
            "VAT16",
            "VAT (Standard 16%)",
            dec(16),
            vat_output,
            TaxDirection::Output,
        ),
        (
            "VAT0",
            "VAT (Zero Rated)",
            Decimal::ZERO,
            vat_output,
            TaxDirection::Output,
        ),
        (
            "EXEMPT",
            "VAT (Exempt)",
            Decimal::ZERO,
            None,
            TaxDirection::Output,
        ),
        (
            "VAT16-IN",
            "Input VAT (Standard 16%)",
            dec(16),
            vat_input,
            TaxDirection::Input,
        ),
    ];
    for (code, name, rate, account_id, direction) in tax_codes {
        tax::ActiveModel {
            id: Set(Uuid::new_v4()),
            code: Set(code.to_string()),
            name: Set(name.to_string()),
            rate: Set(rate),
            account_id: Set(account_id),
            direction: Set(direction.as_str().to_string()),
            is_system: Set(true),
            is_active: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&txn)
        .await?;
    }

    txn.commit().await?;
    Ok(true)
}

async fn system_account_id<C: sea_orm::ConnectionTrait>(
    conn: &C,
    key: &str,
) -> Result<Option<Uuid>> {
    Ok(account::Entity::find()
        .filter(account::Column::SystemKey.eq(key))
        .one(conn)
        .await?
        .map(|a| a.id))
}

fn dec(whole: i64) -> Decimal {
    Decimal::from(whole)
}
