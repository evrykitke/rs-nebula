//! The default chart of accounts and tax codes.
//!
//! Seeded once per tenant database so a business can sell and read reports
//! with zero accounting configuration (think a POS handed to a small
//! retailer). Every seeded account carries a stable [`account::keys`] role
//! so other modules post to it without configuration; a business that
//! cares can rename, deactivate or extend the chart freely.
//!
//! Seeding is idempotent *per account and tax code*: each seeded row is
//! inserted only when its role (system key, or code for headers) is still
//! missing. Running again on every boot rollout is therefore safe — and
//! it is how defaults added in a later release reach existing tenants.

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
                    code: "2150",
                    name: "Goods Received Not Invoiced",
                    system_key: keys::GRNI,
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
            children: &[
                SeedChild {
                    code: "5100",
                    name: "Cost of Goods Sold",
                    system_key: keys::COGS,
                },
                SeedChild {
                    code: "5200",
                    name: "Stock Adjustments",
                    system_key: keys::STOCK_ADJUSTMENT,
                },
                SeedChild {
                    code: "5300",
                    name: "Purchase Price Variance",
                    system_key: keys::PURCHASE_PRICE_VARIANCE,
                },
            ],
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
/// `currency`. Additive and idempotent: each account is inserted only when
/// its role is missing, so a later release's new defaults reach an
/// already-seeded tenant. Returns `true` if anything was inserted.
pub async fn seed_defaults(db: &DatabaseConnection, currency: &str) -> Result<bool> {
    let now = chrono::Utc::now();
    let mut seeded_any = false;
    let txn = db.begin().await?;

    for group in default_accounts() {
        // The header account: a system account with no role, organizing
        // its postable children. Headers have no system key, so they are
        // recognized by code.
        let parent_id = match find_by_code(&txn, group.code).await? {
            Some(existing) => existing.id,
            None => {
                let parent_id = Uuid::new_v4();
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
                seeded_any = true;
                parent_id
            }
        };

        for child in group.children {
            if system_account_id(&txn, child.system_key).await?.is_some() {
                continue;
            }
            // A tenant may have claimed the code for its own account; the
            // role then stays unfulfilled rather than clashing.
            if find_by_code(&txn, child.code).await?.is_some() {
                tracing::warn!(code = %child.code, key = %child.system_key,
                    "seed skipped: account code is already taken");
                continue;
            }
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
            seeded_any = true;
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
        let existing = tax::Entity::find()
            .filter(tax::Column::Code.eq(code))
            .count(&txn)
            .await?;
        if existing > 0 {
            continue;
        }
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
        seeded_any = true;
    }

    txn.commit().await?;
    Ok(seeded_any)
}

/// Re-denominate the whole ledger into `currency`.
///
/// Onboarding creates the company first and asks for its currency after,
/// so the chart has already been seeded in the fallback currency by the
/// time the answer arrives; this is what makes the answer stick. It also
/// serves a company that simply picked the wrong currency on day one.
///
/// Only ever while the ledger is untouched: once anything is posted the
/// amounts *mean* the currency they were booked in, and restating them is
/// an accounting exercise, not a column update. So a ledger with postings
/// is left alone and `false` comes back — the caller decides how loudly to
/// complain. Drafts are re-denominated with the chart: nothing is recorded
/// yet, and leaving them behind would only make them unpostable.
pub async fn redenominate(db: &DatabaseConnection, currency: &str) -> Result<bool> {
    use crate::accounting::journal::{entry, posting};

    let txn = db.begin().await?;
    if posting::Entity::find().count(&txn).await? > 0 {
        return Ok(false);
    }

    let accounts = account::Entity::update_many()
        .col_expr(account::Column::Currency, Expr::value(currency.to_string()))
        .filter(account::Column::Currency.ne(currency))
        .exec(&txn)
        .await?;
    entry::Entity::update_many()
        .col_expr(entry::Column::Currency, Expr::value(currency.to_string()))
        .filter(entry::Column::Currency.ne(currency))
        .exec(&txn)
        .await?;
    txn.commit().await?;
    Ok(accounts.rows_affected > 0)
}

async fn find_by_code<C: sea_orm::ConnectionTrait>(
    conn: &C,
    code: &str,
) -> Result<Option<account::Model>> {
    account::Entity::find()
        .filter(account::Column::Code.eq(code))
        .one(conn)
        .await
        .map_err(nebula::error::Error::from)
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
