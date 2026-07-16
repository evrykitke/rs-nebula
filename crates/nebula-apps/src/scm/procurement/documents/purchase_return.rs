//! The return note that travels back with the goods.

use super::{WithSupplier, supplier_of, trace_of};
use crate::scm::document::{Document, date, quantity};
use crate::scm::procurement::permissions::names;
use crate::scm::procurement::returns::{ReturnService, ReturnView};
use nebula::{
    Column, DataCx, Error, KeyValue, Report, ReportData, ReportDataSource, ReportDefinition,
    ReportOutput, Result, Signature,
};
use std::sync::Arc;

const KEY: &str = "scm_purchase_return_doc";

pub struct ReturnDataSource;

#[async_trait::async_trait]
impl ReportDataSource for ReturnDataSource {
    fn key(&self) -> &'static str {
        KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let record = ReturnService::new(db.clone()).view(cx.params.id()?).await?;
        let supplier_name = supplier_of(cx, record.order_id).await?;
        serde_json::to_value(WithSupplier {
            record,
            supplier_name,
        })
        .map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct PurchaseReturnDocument;

impl ReportDefinition for PurchaseReturnDocument {
    fn name(&self) -> &'static str {
        "purchase-return"
    }
    fn title(&self) -> &'static str {
        "Purchase Return"
    }
    fn group(&self) -> &'static str {
        "Procurement"
    }
    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf]
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::RETURNS_VIEW)
    }
    /// Drawn for one record: without `?id=` there is nothing to draw.
    fn requires_record(&self) -> bool {
        true
    }
    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(ReturnDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let WithSupplier {
            record: r,
            supplier_name,
        } = data.get::<WithSupplier<ReturnView>>(KEY)?;

        let mut meta = vec![KeyValue::new("Return date", date(r.return_date))];
        if let Some(o) = r.order_number.as_deref().filter(|s| !s.trim().is_empty()) {
            meta.push(KeyValue::new("Against order", o));
        }
        for (label, value) in [
            ("Reason", r.reason.as_deref()),
            ("Reference", r.reference.as_deref()),
            ("Carrier", r.carrier.as_deref()),
        ] {
            if let Some(v) = value.filter(|s| !s.trim().is_empty()) {
                meta.push(KeyValue::new(label, v));
            }
        }

        // The lot going back matters more here than anywhere: a recall is
        // why most of these exist.
        let traced = r
            .lines
            .iter()
            .any(|l| l.batch_no.is_some() || !l.serial_nos.is_empty());
        let mut columns = vec![
            Column::new("#"),
            Column::new("SKU"),
            Column::wide("Description"),
            Column::number("Qty"),
            Column::new("Reason"),
        ];
        if traced {
            columns.push(Column::new("Batch / serials"));
        }

        let rows = r
            .lines
            .iter()
            .map(|l| {
                let mut cells = vec![
                    l.line_no.to_string(),
                    l.sku.clone(),
                    l.item_name.clone(),
                    quantity(l.qty),
                    l.reason.clone().unwrap_or_default(),
                ];
                if traced {
                    cells.push(trace_of(l.batch_no.as_deref(), &l.serial_nos));
                }
                cells
            })
            .collect();

        Ok(Document {
            title: "Purchase Return".to_string(),
            number: r.number.clone().into(),
            status: r.status.as_str().replace('_', " "),
            party_label: "Return to",
            party: vec![supplier_name],
            second_label: None,
            second: Vec::new(),
            meta,
            columns,
            rows,
            totals: Vec::new(),
            terms: None,
            memo: r.memo.clone(),
            signatures: vec![
                Signature::new("Returned by").dated(),
                Signature::new("Collected by").dated(),
            ],
            footer_notes: Vec::new(),
        }
        .into_report())
    }
}
