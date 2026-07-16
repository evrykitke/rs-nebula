//! The request for quotation as sent to a supplier — the page that asks what
//! it would cost, before anything is committed to.

use super::status_line;
use crate::scm::document::{Document, date, quantity};
use crate::scm::procurement::permissions::names;
use crate::scm::procurement::rfq::{RfqService, RfqView};
use nebula::{
    Column, DataCx, KeyValue, Report, ReportData, ReportDataSource, ReportDefinition, ReportOutput,
    Result, Signature,
};
use std::sync::Arc;

const KEY: &str = "scm_rfq_doc";

/// Loads the one RFQ the caller asked for.
pub struct RfqDataSource;

#[async_trait::async_trait]
impl ReportDataSource for RfqDataSource {
    fn key(&self) -> &'static str {
        KEY
    }

    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let view = RfqService::new(db.clone()).view(cx.params.id()?).await?;
        serde_json::to_value(view).map_err(|e| nebula::Error::internal(e.to_string()))
    }
}

pub struct RfqDocument;

impl ReportDefinition for RfqDocument {
    fn name(&self) -> &'static str {
        "request-for-quotation"
    }
    fn title(&self) -> &'static str {
        "Request for Quotation"
    }
    fn group(&self) -> &'static str {
        "Procurement"
    }
    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf]
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::RFQS_VIEW)
    }
    /// Drawn for one record: without `?id=` there is nothing to draw.
    fn requires_record(&self) -> bool {
        true
    }
    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(RfqDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let r: RfqView = data.get(KEY)?;

        let mut meta = vec![KeyValue::new("Subject", r.title.clone())];
        if let Some(due) = r.due_date {
            // The whole point of the page: the date an answer is wanted by.
            meta.push(KeyValue::new("Reply by", date(due)));
        }
        if let Some(sent) = r.sent_at {
            meta.push(KeyValue::new("Sent", date(sent.date_naive())));
        }
        if let Some(req) = r.requisition_number.as_deref() {
            meta.push(KeyValue::new("Requisition", req));
        }
        if let Some(order) = r.order_number.as_deref() {
            // Once awarded, the page should say what it became.
            meta.push(KeyValue::new("Awarded order", order));
        }

        // Everyone asked, on the page they are all asked on. An RFQ goes to a
        // field of suppliers by design, so the document names the field rather
        // than pretending to address one of them.
        let party: Vec<String> = r
            .suppliers
            .iter()
            .map(|s| format!("{} — {}", s.code, s.name))
            .collect();

        // A quote form, not a priced document: what is wanted, in what
        // quantity, with the money column left for the supplier to fill in.
        let columns = vec![
            Column::new("#"),
            Column::new("SKU"),
            Column::wide("Description"),
            Column::new("Unit"),
            Column::number("Qty"),
            Column::number("Unit price"),
            Column::number("Lead time"),
        ];

        let rows = r
            .lines
            .iter()
            .map(|l| {
                let mut description = l.item_name.clone();
                if let Some(memo) = l.memo.as_deref().filter(|m| !m.trim().is_empty()) {
                    description.push_str(" — ");
                    description.push_str(memo);
                }
                vec![
                    l.line_no.to_string(),
                    l.sku.clone(),
                    description,
                    l.uom_code.clone(),
                    quantity(l.qty),
                    String::new(),
                    String::new(),
                ]
            })
            .collect();

        Ok(Document {
            title: "Request for Quotation".to_string(),
            number: r.number.clone().into(),
            status: status_line(r.status.as_str(), None),
            party_label: "Invited suppliers",
            party,
            second_label: None,
            second: Vec::new(),
            meta,
            columns,
            // Nothing is priced yet — a total would be an invention.
            totals: Vec::new(),
            rows,
            terms: None,
            memo: r.memo.clone(),
            // Ours on the left, theirs on the right: the page goes out asking
            // and comes back as an answer someone stands behind.
            signatures: vec![
                Signature::new("Prepared by").dated(),
                Signature::new("Quoted by").dated(),
            ],
            footer_notes: Vec::new(),
        }
        .into_report())
    }
}
