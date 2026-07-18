//! The printable Z: one closed session on the tenant's stationery — the
//! paper a shop staples to the day's drawer count. Drawn for one record
//! (`?id=` the session), from the counts stored at close, so it prints the
//! same forever.

use super::queries::{PosQueries, ZView};
use super::{money, qty};
use crate::scm::pos::permissions::names;
use nebula::error::Error;
use nebula::{
    Column as ReportColumn, DataCx, Group, KeyValue, Metric, Report, ReportData, ReportDataSource,
    ReportDefinition, ReportOutput, Result, Table, Widget,
};
use rust_decimal::Decimal;
use std::sync::Arc;

const KEY: &str = "pos_z_report";

pub struct ZReportDataSource;
#[async_trait::async_trait]
impl ReportDataSource for ZReportDataSource {
    fn key(&self) -> &'static str {
        KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let view = PosQueries::new(db.clone()).z(cx.params.id()?).await?;
        serde_json::to_value(view).map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct ZReportDocument;
impl ReportDefinition for ZReportDocument {
    fn name(&self) -> &'static str {
        "pos-z"
    }
    fn title(&self) -> &'static str {
        "Z Report"
    }
    fn group(&self) -> &'static str {
        "Point of Sale"
    }
    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf]
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    /// One session per Z; without `?id=` there is nothing to print.
    fn requires_record(&self) -> bool {
        true
    }
    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(ZReportDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let ZView {
            report: r,
            sheets,
            items,
        } = data.get(KEY)?;
        let s = &r.session;
        let number = s.number.clone().unwrap_or_default();

        let mut meta = vec![
            KeyValue::new("Register", format!("{} — {}", s.register_code, s.register_name)),
            KeyValue::new("Opened", s.opened_at.format("%Y-%m-%d %H:%M").to_string()),
        ];
        if let Some(closed) = s.closed_at {
            meta.push(KeyValue::new(
                "Closed",
                closed.format("%Y-%m-%d %H:%M").to_string(),
            ));
        }
        meta.push(KeyValue::new("Opening float", amount(s.opening_float)));
        if let Some(note) = &s.closing_note {
            meta.push(KeyValue::new("Closing note", note.clone()));
        }

        let metrics = Widget::metrics(vec![
            Metric::new("Gross sales", amount(r.gross_sales))
                .caption(format!("{} sales", r.orders)),
            Metric::new("Refunds", amount(r.refund_total))
                .caption(format!("{} refunds", r.refunds)),
            Metric::new("Net takings", amount(r.net_total)).caption("VAT included"),
            Metric::new("VAT inside", amount(r.tax_total)),
        ]);

        let mut tenders = Table::new(vec![
            ReportColumn::new("Tender"),
            ReportColumn::number("Sales"),
            ReportColumn::number("Refunds"),
            ReportColumn::number("Net"),
            ReportColumn::number("Expected"),
            ReportColumn::number("Counted"),
            ReportColumn::number("Variance"),
        ])
        .title("Tenders");
        for t in &r.tenders {
            tenders = tenders.row([
                t.tender.clone(),
                money(t.sales),
                money(t.refunds),
                money(t.net),
                amount(t.expected),
                t.counted.map(amount).unwrap_or_default(),
                t.variance.map(money).unwrap_or_default(),
            ]);
        }

        let drawer = Widget::key_values(vec![
            KeyValue::new("Paid in", amount(r.paid_in)),
            KeyValue::new("Paid out", amount(r.paid_out)),
            KeyValue::new(
                "Expected cash",
                r.expected_cash.map(amount).unwrap_or_default(),
            ),
        ]);

        let mut widgets = vec![Widget::key_values(meta), metrics, tenders.into_widget(), drawer];

        // The count sheets, when the drawer was counted by denomination.
        for sheet in &sheets {
            let mut t = Table::new(vec![
                ReportColumn::number("Denomination"),
                ReportColumn::number("Count"),
                ReportColumn::number("Value"),
            ])
            .title(format!("Count sheet — {}", sheet.tender));
            let mut total = Decimal::ZERO;
            for line in &sheet.lines {
                let value = line.denom * Decimal::from(line.count);
                total += value;
                t = t.row([
                    line.denom.normalize().to_string(),
                    line.count.to_string(),
                    amount(value),
                ]);
            }
            t = t.totals(["Total".to_string(), String::new(), amount(total)]);
            widgets.push(t.into_widget());
        }

        // What sold, net of refunds — the day's item summary.
        if !items.is_empty() {
            let mut t = Table::new(vec![
                ReportColumn::wide("Item"),
                ReportColumn::number("Qty"),
                ReportColumn::number("Takings"),
            ])
            .title("Items sold");
            for i in &items {
                t = t.row([i.description.clone(), qty(i.qty), money(i.gross)]);
            }
            widgets.push(t.into_widget());
        }

        // The tempo band the UX research asked the Z to carry (02 §7).
        let mut tempo = vec![
            KeyValue::new("Voided orders", r.voids.to_string()),
            KeyValue::new("Offline captures", r.offline.to_string()),
            KeyValue::new("Price drift", r.price_drift.to_string()),
        ];
        if let Some(v) = r.avg_sale_seconds {
            tempo.insert(0, KeyValue::new("Seconds per sale", v.to_string()));
        }
        if let Some(v) = r.avg_sale_inputs {
            tempo.insert(1, KeyValue::new("Inputs per sale", v.to_string()));
        }
        widgets.push(
            Group::new(vec![Widget::key_values(tempo)])
                .title("Till tempo")
                .boxed()
                .into_widget(),
        );

        let mut report = Report::new("Z Report")
            .subtitle(format!("Session {number}"))
            .number(number.clone())
            .file_name(if number.is_empty() {
                "z-report".to_string()
            } else {
                number
            });
        for w in widgets {
            report = report.with(w);
        }
        Ok(report)
    }
}

/// Zero prints as 0.00 here, not blank: on a Z, "no money" is a statement.
fn amount(v: Decimal) -> String {
    format!("{:.2}", v)
}
