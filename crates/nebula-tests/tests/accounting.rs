//! The accounting app's bookkeeping invariants against a live database:
//! seeding, the draft → post → reverse lifecycle, the balancing rule,
//! chart-of-accounts integrity, period control and the ledger reads.
//! Skips when NEBULA_TEST_DATABASE_URL is unset.
//!
//! One test drives every case: the accounting tables are shared per
//! database, so splitting into parallel tests would race on the seed.

use chrono::Datelike;
use nebula::config::{Config, DatabaseConfig, MigrationsConfig};
use nebula::{Kernel, Module, ModuleContext, Reset, SeriesDef};
use nebula_apps::accounting::expense::{ExpenseService, NewExpense};
use nebula_apps::accounting::journal::{EntryStatus, Ledger, NewEntry, PostingInput};
use nebula_apps::accounting::ledger::LedgerQueries;
use nebula_apps::accounting::{account, fiscal, seed, tax};
use rust_decimal::Decimal;
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, PaginatorTrait, QueryFilter};
use uuid::Uuid;

/// Declares the journal number series without registering the whole app,
/// so seeding is driven explicitly by the test instead of a background
/// rollout task.
struct SeriesOnly;

impl Module for SeriesOnly {
    fn name(&self) -> &'static str {
        "accounting-series-test"
    }

    fn configure(&self, ctx: &mut ModuleContext) {
        ctx.declare_series(
            SeriesDef::new(
                "accounting.journal",
                "Journal Entry",
                "JV-{YYYY}-{SEQ:5}",
                Reset::Yearly,
            )
            .expect("valid series template"),
        );
        ctx.declare_series(
            SeriesDef::new(
                "accounting.expense",
                "Expense Voucher",
                "PV-{YYYY}-{SEQ:5}",
                Reset::Yearly,
            )
            .expect("valid series template"),
        );
    }
}

fn debit(account_id: Uuid, amount: i64) -> PostingInput {
    PostingInput {
        account_id,
        debit: Decimal::from(amount),
        credit: Decimal::ZERO,
        memo: None,
    }
}

fn credit(account_id: Uuid, amount: i64) -> PostingInput {
    PostingInput {
        account_id,
        debit: Decimal::ZERO,
        credit: Decimal::from(amount),
        memo: None,
    }
}

fn entry(
    date: chrono::NaiveDate,
    memo: &str,
    lines: Vec<PostingInput>,
) -> NewEntry {
    NewEntry {
        entry_date: date,
        memo: memo.into(),
        reference: None,
        currency: "USD".into(),
        lines,
        created_by: None,
    }
}

#[tokio::test]
async fn bookkeeping_invariants_end_to_end() {
    let Ok(url) = std::env::var("NEBULA_TEST_DATABASE_URL") else {
        eprintln!("SKIPPED: set NEBULA_TEST_DATABASE_URL to run database tests");
        return;
    };

    // Clean slate for the accounting module only; the framework schema is
    // idempotent under auto_migrate.
    let admin_db = nebula::db::connect(&DatabaseConfig {
        url: url.as_str().into(),
        ..DatabaseConfig::default()
    })
    .await
    .expect("must connect");
    admin_db
        .execute_unprepared(
            "DROP TABLE IF EXISTS accounting_postings; \
             DROP TABLE IF EXISTS accounting_journal_entries; \
             DROP TABLE IF EXISTS accounting_tax_codes; \
             DROP TABLE IF EXISTS accounting_accounts; \
             DROP TABLE IF EXISTS accounting_fiscal_periods; \
             DROP TABLE IF EXISTS accounting_fiscal_years; \
             DO $$ BEGIN IF to_regclass('public.nebula_sql_migrations') IS NOT NULL THEN \
               DELETE FROM nebula_sql_migrations WHERE module = 'accounting'; \
             END IF; END $$;",
        )
        .await
        .expect("cleanup must work");

    let mut config = Config::default();
    config.database = DatabaseConfig {
        url: url.as_str().into(),
        auto_migrate: true,
        ..DatabaseConfig::default()
    };
    // The repo's real migration files, wherever the test runs from.
    config.migrations = MigrationsConfig {
        root: format!("{}/../../migrations", env!("CARGO_MANIFEST_DIR")),
    };

    let app = Kernel::builder()
        .with_config(config)
        .add_module(SeriesOnly)
        .build()
        .expect("kernel must build")
        .init()
        .await
        .expect("boot must succeed");
    let db = app.database().expect("database must exist").clone();
    let numbering = app.numbering();

    // --- seeding: once, then a no-op ---
    assert!(seed::seed_defaults(&db, "USD").await.unwrap());
    assert!(
        !seed::seed_defaults(&db, "USD").await.unwrap(),
        "second seed must be a no-op"
    );
    assert!(fiscal::FiscalService::new(db.clone())
        .ensure_current_year()
        .await
        .unwrap());

    // --- the currency picked after the company exists (onboarding step two) ---
    // The chart is seeded in the fallback currency, so a currency chosen later
    // has to reach the accounts — otherwise nothing could ever be posted in it.
    assert!(seed::redenominate(&db, "KES").await.unwrap());
    assert_eq!(
        account::Entity::find()
            .filter(account::Column::Currency.ne("KES"))
            .count(&db)
            .await
            .unwrap(),
        0,
        "every account must follow the company's currency"
    );
    assert!(
        !seed::redenominate(&db, "KES").await.unwrap(),
        "re-denominating to the currency already in force changes nothing"
    );
    // Back to USD, which the rest of this test books in.
    assert!(seed::redenominate(&db, "USD").await.unwrap());

    let accounts = account::Store::new(db.clone());
    let cash = accounts
        .find_by_system_key(account::keys::CASH)
        .await
        .unwrap()
        .expect("seeded cash account");
    let sales = accounts
        .find_by_system_key(account::keys::SALES)
        .await
        .unwrap()
        .expect("seeded sales account");
    let opex = accounts
        .find_by_system_key(account::keys::OPEX)
        .await
        .unwrap()
        .expect("seeded opex account");
    let assets_header = account::Entity::find()
        .filter(account::Column::Code.eq("1000"))
        .one(&db)
        .await
        .unwrap()
        .expect("seeded assets header");

    let ledger = Ledger::new(db.clone());
    let today = chrono::Utc::now().date_naive();

    // --- draft → post: number allocated, entry frozen ---
    let draft = ledger
        .create_draft(entry(today, "Cash sale", vec![debit(cash.id, 100), credit(sales.id, 100)]))
        .await
        .unwrap();
    assert_eq!(draft.status, EntryStatus::Draft);
    assert!(draft.number.is_none(), "a draft is unnumbered");
    let posted = ledger.post(draft.id, &numbering).await.unwrap();
    assert_eq!(posted.status, EntryStatus::Posted);
    let number = posted.number.clone().expect("posting allocates a number");
    assert!(number.starts_with("JV-"), "series format: {number}");
    assert_eq!(posted.total_debit, posted.total_credit);

    // A posted entry can be neither edited, deleted nor posted again.
    assert!(ledger
        .update_draft(posted.id, entry(today, "tamper", vec![debit(cash.id, 1), credit(sales.id, 1)]))
        .await
        .is_err());
    assert!(ledger.delete_draft(posted.id).await.is_err());
    assert!(ledger.post(posted.id, &numbering).await.is_err());

    // --- the balancing rule is enforced at post time ---
    let unbalanced = ledger
        .create_draft(entry(today, "Unbalanced", vec![debit(cash.id, 100), credit(sales.id, 90)]))
        .await
        .unwrap();
    assert!(
        ledger.post(unbalanced.id, &numbering).await.is_err(),
        "an unbalanced entry must not post"
    );

    // --- line validation ---
    // Header accounts (they have sub-accounts) never carry postings.
    assert!(ledger
        .create_draft(entry(today, "To header", vec![debit(assets_header.id, 10), credit(sales.id, 10)]))
        .await
        .is_err());
    // Amounts beyond two decimal places would be silently rounded by the
    // column type, so they are rejected.
    let sub_cent = PostingInput {
        account_id: cash.id,
        debit: Decimal::new(10005, 3), // 10.005
        credit: Decimal::ZERO,
        memo: None,
    };
    assert!(ledger
        .create_draft(entry(today, "Sub-cent", vec![sub_cent, credit(sales.id, 10)]))
        .await
        .is_err());
    // Both sides on one line, negative amounts, single-line entries: rejected.
    let both = PostingInput {
        account_id: cash.id,
        debit: Decimal::from(5),
        credit: Decimal::from(5),
        memo: None,
    };
    assert!(ledger
        .create_draft(entry(today, "Both sides", vec![both, credit(sales.id, 5)]))
        .await
        .is_err());
    assert!(ledger
        .create_draft(entry(today, "One line", vec![debit(cash.id, 5)]))
        .await
        .is_err());

    // --- drafts are editable and deletable ---
    let editable = ledger
        .create_draft(entry(today, "Before edit", vec![debit(cash.id, 30), credit(sales.id, 30)]))
        .await
        .unwrap();
    let edited = ledger
        .update_draft(
            editable.id,
            entry(today, "After edit", vec![debit(cash.id, 40), credit(sales.id, 40)]),
        )
        .await
        .unwrap();
    assert_eq!(edited.memo, "After edit");
    assert_eq!(edited.total_debit, Decimal::from(40));
    let deleted = ledger.delete_draft(editable.id).await.unwrap();
    assert_eq!(deleted.id, editable.id);
    assert!(ledger.view(editable.id).await.is_err(), "the draft is gone");

    // --- a stale draft re-validates at post time ---
    let stale = ledger
        .create_draft(entry(today, "Stale", vec![debit(opex.id, 20), credit(cash.id, 20)]))
        .await
        .unwrap();
    accounts
        .update(opex.id, None, None, Some(false))
        .await
        .unwrap();
    assert!(
        ledger.post(stale.id, &numbering).await.is_err(),
        "a draft touching a deactivated account must not post"
    );
    accounts.update(opex.id, None, None, Some(true)).await.unwrap();

    // --- reversal: mirror entry, one-shot, never dated before the original ---
    assert!(
        ledger
            .reverse(posted.id, "backdated", today.pred_opt(), &numbering, None)
            .await
            .is_err(),
        "a reversal cannot predate its original"
    );
    let reversal = ledger
        .reverse(posted.id, "wrong amount", None, &numbering, None)
        .await
        .unwrap();
    assert_eq!(reversal.status, EntryStatus::Posted);
    assert_eq!(reversal.reverses_id, Some(posted.id));
    let original = ledger.view(posted.id).await.unwrap();
    assert_eq!(original.status, EntryStatus::Reversed);
    assert_eq!(original.reversed_by_id, Some(reversal.id));
    // The mirror swaps sides: cash was debited 100, now credited 100.
    let cash_line = reversal
        .lines
        .iter()
        .find(|l| l.account_id == cash.id)
        .unwrap();
    assert_eq!(cash_line.credit, Decimal::from(100));
    assert!(
        ledger.reverse(posted.id, "again", None, &numbering, None).await.is_err(),
        "an entry reverses at most once"
    );

    // --- ledger reads: reversal pair nets to zero, books stay balanced ---
    let queries = LedgerQueries::new(db.clone());
    let tb = queries.trial_balance(None).await.unwrap();
    assert_eq!(tb.total_debit, tb.total_credit, "the trial balance must foot");
    let cash_ledger = queries.account_ledger(cash.id, None, None).await.unwrap();
    assert!(
        cash_ledger.lines.len() >= 2,
        "both sides of the reversal pair stay in the ledger"
    );
    assert_eq!(
        cash_ledger.closing_balance,
        Decimal::ZERO,
        "the reversal pair nets the account to zero"
    );

    // --- balance sheet: prior-year income presents as retained earnings ---
    let last_year_end =
        chrono::NaiveDate::from_ymd_opt(today.year() - 1, 12, 31).unwrap();
    let prior_sale = ledger
        .create_draft(entry(
            last_year_end,
            "Prior-year sale",
            vec![debit(cash.id, 50), credit(sales.id, 50)],
        ))
        .await
        .unwrap();
    ledger.post(prior_sale.id, &numbering).await.unwrap();
    let this_year_sale = ledger
        .create_draft(entry(today, "This-year sale", vec![debit(cash.id, 7), credit(sales.id, 7)]))
        .await
        .unwrap();
    ledger.post(this_year_sale.id, &numbering).await.unwrap();

    let bs = queries.balance_sheet(None).await.unwrap();
    assert!(bs.balanced, "assets must equal liabilities + equity + earnings");
    assert_eq!(bs.total_assets, bs.total_liabilities_and_equity);
    assert_eq!(
        bs.prior_earnings,
        Decimal::from(50),
        "last year's income is retained earnings"
    );
    assert_eq!(
        bs.current_earnings,
        Decimal::from(7),
        "only this fiscal year's income is current"
    );

    let is = queries
        .income_statement(
            chrono::NaiveDate::from_ymd_opt(today.year(), 1, 1),
            Some(today),
        )
        .await
        .unwrap();
    assert_eq!(is.net_income, Decimal::from(7), "period statements only see the period");

    // --- period control: a closed period rejects postings until reopened ---
    let years = fiscal::FiscalService::new(db.clone()).list().await.unwrap();
    let year = years.first().expect("current fiscal year");
    let current_period = year
        .periods
        .iter()
        .find(|p| p.start_date <= today && today <= p.end_date)
        .expect("a period covers today");
    let other_period = year
        .periods
        .iter()
        .find(|p| p.id != current_period.id)
        .expect("eleven other periods");
    let fiscal_svc = fiscal::FiscalService::new(db.clone());

    fiscal_svc.close_period(current_period.id).await.unwrap();
    let blocked = ledger
        .create_draft(entry(today, "Blocked", vec![debit(cash.id, 5), credit(sales.id, 5)]))
        .await
        .unwrap();
    assert!(
        ledger.post(blocked.id, &numbering).await.is_err(),
        "posting into a closed period must fail"
    );
    fiscal_svc.reopen_period(current_period.id).await.unwrap();
    ledger.post(blocked.id, &numbering).await.unwrap();

    // Locking is permanent: only a closed period locks, and a locked one
    // never reopens.
    assert!(fiscal_svc.lock_period(other_period.id).await.is_err());
    fiscal_svc.close_period(other_period.id).await.unwrap();
    fiscal_svc.lock_period(other_period.id).await.unwrap();
    assert!(fiscal_svc.reopen_period(other_period.id).await.is_err());

    // --- chart-of-accounts integrity ---
    // A sub-account must match its parent's type, and a posted account
    // cannot become a header.
    assert!(accounts
        .create(account::NewAccount {
            code: "1999".into(),
            name: "Mismatched child".into(),
            account_type: account::AccountType::Revenue,
            currency: "USD".into(),
            parent_id: Some(assets_header.id),
            description: None,
        })
        .await
        .is_err());
    assert!(accounts
        .create(account::NewAccount {
            code: "1011".into(),
            name: "Child of posted leaf".into(),
            account_type: account::AccountType::Asset,
            currency: "USD".into(),
            parent_id: Some(cash.id),
            description: None,
        })
        .await
        .is_err());
    // Codes are unique; posted and system accounts are undeletable.
    assert!(accounts
        .create(account::NewAccount {
            code: cash.code.clone(),
            name: "Duplicate".into(),
            account_type: account::AccountType::Asset,
            currency: "USD".into(),
            parent_id: None,
            description: None,
        })
        .await
        .is_err());
    assert!(accounts.delete(cash.id).await.is_err());

    // An account referenced by a tax code cannot be deleted until unlinked.
    let fee_account = accounts
        .create(account::NewAccount {
            code: "2400".into(),
            name: "Levy Payable".into(),
            account_type: account::AccountType::Liability,
            currency: "USD".into(),
            parent_id: None,
            description: None,
        })
        .await
        .unwrap();
    let taxes = tax::Store::new(db.clone());
    // A tax code cannot point at a nonexistent account.
    assert!(taxes
        .create(tax::NewTaxCode {
            code: "GHOST".into(),
            name: "Ghost".into(),
            rate: Decimal::from(1),
            account_id: Some(Uuid::new_v4()),
            direction: tax::TaxDirection::Output,
        })
        .await
        .is_err());
    let levy = taxes
        .create(tax::NewTaxCode {
            code: "LEVY2".into(),
            name: "Levy 2%".into(),
            rate: Decimal::from(2),
            account_id: Some(fee_account.id),
            direction: tax::TaxDirection::Output,
        })
        .await
        .unwrap();
    assert!(accounts.delete(fee_account.id).await.is_err());
    taxes.delete(levy.id).await.unwrap();
    accounts.delete(fee_account.id).await.unwrap();

    // Tax math: percentage of a base, banker's rounding to cents.
    let vat = tax::Entity::find()
        .filter(tax::Column::Code.eq("VAT16"))
        .one(&db)
        .await
        .unwrap()
        .expect("seeded VAT16");
    assert_eq!(vat.tax_on(Decimal::from(100)), Decimal::new(1600, 2));
    assert_eq!(vat.tax_on(Decimal::new(1031, 2)), Decimal::new(165, 2)); // 16% of 10.31 = 1.6496

    // --- expense recording: one call books the balanced voucher ---
    let vat_in = tax::Entity::find()
        .filter(tax::Column::Code.eq("VAT16-IN"))
        .one(&db)
        .await
        .unwrap()
        .expect("seeded input VAT");
    let expenses = ExpenseService::new(db.clone());
    let voucher = expenses
        .record(
            NewExpense {
                entry_date: today,
                memo: "Office stationery".into(),
                reference: Some("TILL-042".into()),
                expense_account_id: opex.id,
                payment_account_id: cash.id,
                amount: Decimal::from(100),
                tax_code_id: Some(vat_in.id),
                created_by: None,
            },
            &numbering,
        )
        .await
        .unwrap();
    assert_eq!(voucher.status, EntryStatus::Posted);
    assert!(
        voucher.number.as_deref().unwrap_or("").starts_with("PV-"),
        "expense vouchers get their own series: {:?}",
        voucher.number
    );
    // Debit expense 100, debit recoverable VAT 16, credit cash 116.
    assert_eq!(voucher.total_debit, Decimal::from(116));
    let cash_credit = voucher
        .lines
        .iter()
        .find(|l| l.account_id == cash.id)
        .unwrap();
    assert_eq!(cash_credit.credit, Decimal::from(116));

    // Role checks: the "what for" account must be an expense, the payer an
    // asset, and the tax code an input code.
    let bad_expense = expenses
        .record(
            NewExpense {
                entry_date: today,
                memo: "Paid into sales?".into(),
                reference: None,
                expense_account_id: sales.id,
                payment_account_id: cash.id,
                amount: Decimal::from(10),
                tax_code_id: None,
                created_by: None,
            },
            &numbering,
        )
        .await;
    assert!(bad_expense.is_err(), "a revenue account is not an expense");
    let output_vat = tax::Entity::find()
        .filter(tax::Column::Code.eq("VAT16"))
        .one(&db)
        .await
        .unwrap()
        .expect("seeded output VAT");
    let wrong_direction = expenses
        .record(
            NewExpense {
                entry_date: today,
                memo: "Wrong tax direction".into(),
                reference: None,
                expense_account_id: opex.id,
                payment_account_id: cash.id,
                amount: Decimal::from(10),
                tax_code_id: Some(output_vat.id),
                created_by: None,
            },
            &numbering,
        )
        .await;
    assert!(wrong_direction.is_err(), "sales VAT cannot book an expense");

    // The voucher shows up in the expense list and the books still foot.
    let listed = expenses.list().await.unwrap();
    assert!(listed.iter().any(|e| e.id == voucher.id));
    let tb = queries.trial_balance(None).await.unwrap();
    assert_eq!(tb.total_debit, tb.total_credit);

    // Entry currency must match every line's account currency.
    let eur_account = accounts
        .create(account::NewAccount {
            code: "1090".into(),
            name: "EUR Cash".into(),
            account_type: account::AccountType::Asset,
            currency: "EUR".into(),
            parent_id: None,
            description: None,
        })
        .await
        .unwrap();
    assert!(ledger
        .create_draft(entry(today, "Mixed currency", vec![debit(eur_account.id, 5), credit(sales.id, 5)]))
        .await
        .is_err());

    // --- a ledger in use is never re-denominated ---
    // Posted amounts mean the currency they were booked in; restating them is
    // an accounting exercise, not a column update.
    assert!(
        !seed::redenominate(&db, "KES").await.unwrap(),
        "a ledger with postings must not be re-denominated"
    );
    assert_eq!(
        accounts.find_by_system_key(account::keys::CASH).await.unwrap().unwrap().currency,
        "USD",
        "the accounts must be left exactly as they were booked"
    );
}
