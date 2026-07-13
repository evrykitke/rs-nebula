//! Expense recording: the everyday "we paid for something" operation.
//!
//! A bookkeeper-free front door to the journal: the caller names what the
//! money was for (an expense account), how it was paid (an asset account
//! — cash, bank, a petty-cash float), the amount and an optional input
//! tax code. The service builds the balanced entry — debit expense, debit
//! recoverable VAT, credit the payment account — and posts it immediately
//! under the `accounting.expense` (`PV-`) voucher series. The result is a
//! regular posted journal entry: it appears in the register and every
//! report, and is corrected by reversal like any other.

use crate::accounting::journal::{self, JournalEntryHeader, JournalEntryView, Ledger};
use crate::accounting::permissions::names;
use crate::accounting::{account, tax};
use axum::routing::get;
use axum::{Extension, Json, Router};
use nebula::audit::Audit;
use nebula::auth::Authz;
use nebula::error::{Error, Result};
use nebula::{Numbering, TenantDb, sea_orm};
use rust_decimal::Decimal;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder};
use serde::Deserialize;
use uuid::Uuid;

/// A new expense as supplied by a caller.
pub struct NewExpense {
    pub entry_date: chrono::NaiveDate,
    /// What the money was for, e.g. "Office stationery".
    pub memo: String,
    /// External document (a till slip, an invoice number).
    pub reference: Option<String>,
    /// The expense account this spend belongs to.
    pub expense_account_id: Uuid,
    /// The asset account the money left (cash, bank, a petty-cash float).
    pub payment_account_id: Uuid,
    /// The net amount, before tax.
    pub amount: Decimal,
    /// An input (purchase) tax code, when the spend carries recoverable tax.
    pub tax_code_id: Option<Uuid>,
    pub created_by: Option<Uuid>,
}

/// Records expenses on one (tenant) connection.
pub struct ExpenseService {
    db: DatabaseConnection,
}

impl ExpenseService {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Record an expense: validate the accounts play their roles, compute
    /// the tax, and post the balanced entry in one step.
    pub async fn record(&self, new: NewExpense, numbering: &Numbering) -> Result<JournalEntryView> {
        if new.amount <= Decimal::ZERO {
            return Err(Error::Validation(
                "the expense amount must be greater than zero".into(),
            ));
        }
        let accounts = account::Store::new(self.db.clone());
        let expense = accounts.find_by_id(new.expense_account_id).await?;
        if account::AccountType::parse(&expense.account_type)? != account::AccountType::Expense {
            return Err(Error::Validation(format!(
                "account {} is not an expense account",
                expense.code
            )));
        }
        let payment = accounts.find_by_id(new.payment_account_id).await?;
        if account::AccountType::parse(&payment.account_type)? != account::AccountType::Asset {
            return Err(Error::Validation(format!(
                "account {} cannot pay an expense; pick a cash or bank (asset) account",
                payment.code
            )));
        }

        let mut lines = vec![journal::PostingInput {
            account_id: expense.id,
            debit: new.amount,
            credit: Decimal::ZERO,
            memo: None,
        }];
        let mut total = new.amount;
        if let Some(tax_code_id) = new.tax_code_id {
            let code = tax::Store::new(self.db.clone()).find_by_id(tax_code_id).await?;
            if !code.is_active {
                return Err(Error::Validation(format!(
                    "tax code {} is inactive",
                    code.code
                )));
            }
            if tax::TaxDirection::parse(&code.direction)? != tax::TaxDirection::Input {
                return Err(Error::Validation(format!(
                    "tax code {} is an output (sales) code; an expense needs an input (purchase) code",
                    code.code
                )));
            }
            let tax_amount = code.tax_on(new.amount);
            if tax_amount > Decimal::ZERO {
                let Some(tax_account_id) = code.account_id else {
                    return Err(Error::Validation(format!(
                        "tax code {} has no account to post its tax to",
                        code.code
                    )));
                };
                lines.push(journal::PostingInput {
                    account_id: tax_account_id,
                    debit: tax_amount,
                    credit: Decimal::ZERO,
                    memo: Some(format!("{} on {}", code.code, new.memo.trim())),
                });
                total += tax_amount;
            }
        }
        lines.push(journal::PostingInput {
            account_id: payment.id,
            debit: Decimal::ZERO,
            credit: total,
            memo: None,
        });

        Ledger::new(self.db.clone())
            .create_posted(
                journal::NewEntry {
                    entry_date: new.entry_date,
                    memo: new.memo,
                    reference: new.reference,
                    currency: expense.currency,
                    lines,
                    created_by: new.created_by,
                },
                super::EXPENSE_SERIES,
                numbering,
            )
            .await
    }

    /// The recorded expense vouchers (entries numbered from the expense
    /// series), newest first.
    pub async fn list(&self) -> Result<Vec<JournalEntryHeader>> {
        let rows = journal::entry::Entity::find()
            .filter(journal::entry::Column::Number.like(format!("{EXPENSE_PREFIX}%")))
            .order_by_desc(journal::entry::Column::EntryDate)
            .order_by_desc(journal::entry::Column::CreatedAt)
            .all(&self.db)
            .await?;
        Ledger::new(self.db.clone()).headers(rows).await
    }
}

/// The number prefix of the expense voucher series template.
const EXPENSE_PREFIX: &str = "PV-";

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

pub(super) fn routes() -> Router {
    Router::new().route(
        "/accounting/expenses",
        get(list_expenses).post(record_expense),
    )
}

pub(super) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(list_expenses, record_expense))]
struct ApiDoc;

#[derive(Deserialize, utoipa::ToSchema)]
pub struct RecordExpenseRequest {
    #[schema(value_type = String, format = Date)]
    pub entry_date: chrono::NaiveDate,
    /// What the money was for, e.g. "Office stationery".
    pub memo: String,
    /// External document (a till slip, an invoice number).
    pub reference: Option<String>,
    pub expense_account_id: Uuid,
    /// The asset account the money left (cash, bank, a petty-cash float).
    pub payment_account_id: Uuid,
    /// The net amount, before tax.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub amount: Decimal,
    /// An input (purchase) tax code, when the spend carries recoverable tax.
    pub tax_code_id: Option<Uuid>,
}

#[utoipa::path(get, path = "/accounting/expenses", tag = "accounting",
    responses((status = 200, body = Vec<JournalEntryHeader>)))]
async fn list_expenses(
    authz: Authz,
    TenantDb(db): TenantDb,
) -> Result<Json<Vec<JournalEntryHeader>>> {
    authz.require(names::EXPENSES_VIEW).await?;
    ExpenseService::new(db).list().await.map(Json)
}

#[utoipa::path(post, path = "/accounting/expenses", tag = "accounting",
    request_body = RecordExpenseRequest,
    responses((status = 200, body = JournalEntryView)))]
async fn record_expense(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Extension(numbering): Extension<Numbering>,
    Json(req): Json<RecordExpenseRequest>,
) -> Result<Json<JournalEntryView>> {
    authz.require(names::EXPENSES_RECORD).await?;
    let view = ExpenseService::new(db)
        .record(
            NewExpense {
                entry_date: req.entry_date,
                memo: req.memo,
                reference: req.reference,
                expense_account_id: req.expense_account_id,
                payment_account_id: req.payment_account_id,
                amount: req.amount,
                tax_code_id: req.tax_code_id,
                created_by: Some(authz.user.id),
            },
            &numbering,
        )
        .await?;
    audit.0.created("accounting.expense", view.id, &view).await;
    Ok(Json(view))
}
