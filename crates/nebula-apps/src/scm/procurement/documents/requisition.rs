//! The requisition: an internal ask, carrying the approval it needs.

use super::status_line;
use crate::scm::document::{Document, date, date_opt, quantity};
use crate::scm::procurement::permissions::names;
use crate::scm::procurement::requisition::{RequisitionService, RequisitionView};
use nebula::{
    Column, DataCx, Error, KeyValue, Report, ReportData, ReportDataSource, ReportDefinition,
    ReportOutput, Result, Signature,
};
use std::sync::Arc;

const KEY: &str = "scm_requisition_doc";

pub struct RequisitionDataSource;

#[async_trait::async_trait]
impl ReportDataSource for RequisitionDataSource {
    fn key(&self) -> &'static str {
        KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let view = RequisitionService::new(db.clone())
            .view(cx.params.id()?)
            .await?;
        serde_json::to_value(view).map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct RequisitionDocument;

impl ReportDefinition for RequisitionDocument {
    fn name(&self) -> &'static str {
        "requisition"
    }
    fn title(&self) -> &'static str {
        "Purchase Requisition"
    }
    fn group(&self) -> &'static str {
        "Procurement"
    }
    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf]
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REQUISITIONS_VIEW)
    }
    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(RequisitionDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let r: RequisitionView = data.get(KEY)?;

        let mut meta = Vec::new();
        if let Some(n) = r.needed_by {
            meta.push(KeyValue::new("Needed by", date(n)));
        }
        if let Some(o) = r.order_number.as_deref().filter(|s| !s.trim().is_empty()) {
            // Where the ask ended up: the reader's next question.
            meta.push(KeyValue::new("Became order", o));
        }

        // No prices: a requisition asks for goods, it does not agree a price.
        // The line's own date can differ from the header's, and the store
        // needs to see which.
        let rows = r
            .lines
            .iter()
            .map(|l| {
                vec![
                    l.line_no.to_string(),
                    l.sku.clone(),
                    l.item_name.clone(),
                    quantity(l.qty),
                    l.uom_code.clone(),
                    date_opt(l.needed_by),
                ]
            })
            .collect();

        Ok(Document {
            title: "Purchase Requisition".to_string(),
            number: r.number.clone().into(),
            status: status_line(r.status.as_str(), r.reject_reason.as_deref()),
            party_label: "For",
            party: vec![format!("Warehouse {}", r.warehouse_code)],
            second_label: None,
            second: Vec::new(),
            meta,
            columns: vec![
                Column::new("#"),
                Column::new("SKU"),
                Column::wide("Item"),
                Column::number("Qty"),
                Column::new("UoM"),
                Column::new("Needed by"),
            ],
            rows,
            totals: Vec::new(),
            terms: None,
            memo: r.memo.clone(),
            signatures: vec![
                Signature::new("Requested by").dated(),
                Signature::new("Approved by").dated(),
            ],
        }
        .into_report())
    }
}
