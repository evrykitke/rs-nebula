//! The quotation: an offer with an expiry on it.

use super::{Addressed, party_block, party_of, status_line};
use crate::scm::document::{Document, amount, date, quantity, total_line};
use crate::scm::sales::permissions::names;
use crate::scm::sales::quotation::{QuotationService, QuotationView};
use nebula::{
    Column, DataCx, Error, KeyValue, Report, ReportData, ReportDataSource, ReportDefinition,
    ReportOutput, Result,
};
use std::sync::Arc;

const KEY: &str = "scm_quotation_doc";

pub struct QuotationDataSource;

#[async_trait::async_trait]
impl ReportDataSource for QuotationDataSource {
    fn key(&self) -> &'static str {
        KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let record = QuotationService::new(db.clone())
            .view(cx.params.id()?)
            .await?;
        let party = party_of(cx, record.customer_id, &record.customer_name).await?;
        serde_json::to_value(Addressed { record, party })
            .map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct QuotationDocument;

impl ReportDefinition for QuotationDocument {
    fn name(&self) -> &'static str {
        "quotation"
    }
    fn title(&self) -> &'static str {
        "Quotation"
    }
    fn group(&self) -> &'static str {
        "Sales"
    }
    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf]
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::QUOTATIONS_VIEW)
    }
    /// Drawn for one record: without `?id=` there is nothing to draw.
    fn requires_record(&self) -> bool {
        true
    }
    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(QuotationDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let Addressed { record: q, party } = data.get::<Addressed<QuotationView>>(KEY)?;

        let mut meta = vec![
            KeyValue::new("Date", date(q.quote_date)),
            KeyValue::new("Currency", q.currency.clone()),
        ];
        if let Some(v) = q.valid_until {
            // The one date that decides whether this page still means
            // anything.
            meta.push(KeyValue::new("Valid until", date(v)));
        }

        let discounted = q.lines.iter().any(|l| l.discount_pct.is_some());
        let mut columns = vec![
            Column::new("#"),
            Column::new("SKU"),
            Column::wide("Description"),
            Column::number("Qty"),
            Column::number("Unit price"),
        ];
        if discounted {
            columns.push(Column::number("Disc %"));
        }
        columns.push(Column::number("Net"));

        let rows = q
            .lines
            .iter()
            .map(|l| {
                let mut cells = vec![
                    l.line_no.to_string(),
                    l.sku.clone(),
                    l.description.clone().unwrap_or_else(|| l.item_name.clone()),
                    quantity(l.qty),
                    amount(l.unit_price),
                ];
                if discounted {
                    cells.push(l.discount_pct.map(amount).unwrap_or_default());
                }
                cells.push(amount(l.net));
                cells
            })
            .collect();

        let mut totals = vec![total_line("Subtotal", q.subtotal)];
        if let Some(pct) = q.discount_pct.filter(|d| !d.is_zero()) {
            totals.push(KeyValue::new("Discount", format!("{}%", amount(pct))));
        }
        if let Some(a) = q.discount_amount.filter(|d| !d.is_zero()) {
            totals.push(total_line("Discount", a));
        }
        if let Some(c) = q.other_charges.filter(|d| !d.is_zero()) {
            totals.push(total_line("Other charges", c));
        }
        totals.push(total_line(&format!("Total ({})", q.currency), q.total));

        Ok(Document {
            title: "Quotation".to_string(),
            number: q.number.clone().into(),
            status: status_line(q.status.as_str(), None),
            party_label: "Quoted to",
            party: party_block(&party, &party.billing),
            second_label: None,
            second: Vec::new(),
            meta,
            columns,
            rows,
            totals,
            terms: q.terms_and_conditions.clone(),
            memo: q.memo.clone(),
            signatures: Vec::new(),
            footer_notes: Vec::new(),
        }
        .into_report())
    }
}
