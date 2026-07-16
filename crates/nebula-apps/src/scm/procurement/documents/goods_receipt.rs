//! The goods received note: what actually turned up, against what was
//! ordered.

use super::{WithSupplier, supplier_of, trace_of};
use crate::scm::document::{Document, date, quantity};
use crate::scm::procurement::permissions::names;
use crate::scm::procurement::receipt::{ReceiptService, ReceiptView};
use nebula::{
    Column, DataCx, Error, KeyValue, Report, ReportData, ReportDataSource, ReportDefinition,
    ReportOutput, Result, Signature,
};
use std::sync::Arc;

const KEY: &str = "scm_goods_receipt_doc";

pub struct ReceiptDataSource;

#[async_trait::async_trait]
impl ReportDataSource for ReceiptDataSource {
    fn key(&self) -> &'static str {
        KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let record = ReceiptService::new(db.clone()).view(cx.params.id()?).await?;
        let supplier_name = supplier_of(cx, record.order_id).await?;
        serde_json::to_value(WithSupplier { record, supplier_name })
            .map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct GoodsReceiptDocument;

impl ReportDefinition for GoodsReceiptDocument {
    fn name(&self) -> &'static str {
        "goods-receipt"
    }
    fn title(&self) -> &'static str {
        "Goods Received Note"
    }
    fn group(&self) -> &'static str {
        "Procurement"
    }
    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf]
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::RECEIPTS_VIEW)
    }
    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(ReceiptDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let WithSupplier { record: r, supplier_name } =
            data.get::<WithSupplier<ReceiptView>>(KEY)?;

        let mut meta = vec![KeyValue::new("Received", date(r.receipt_date))];
        if let Some(o) = r.order_number.as_deref().filter(|s| !s.trim().is_empty()) {
            meta.push(KeyValue::new("Against order", o));
        }
        for (label, value) in [
            ("Reference", r.reference.as_deref()),
            ("Carrier", r.carrier.as_deref()),
            ("Tracking", r.tracking_no.as_deref()),
            ("Vehicle", r.vehicle_reg.as_deref()),
            ("Delivered by", r.delivered_by.as_deref()),
        ] {
            if let Some(v) = value.filter(|s| !s.trim().is_empty()) {
                meta.push(KeyValue::new(label, v));
            }
        }

        // A GRN carries no prices: it records what arrived, and the storeman
        // checking it in has no business seeing what it cost. Rejections do
        // belong here — that is the dispute, in writing, at the gate.
        let rejected = r.lines.iter().any(|l| !l.rejected_qty.is_zero());
        let traced = r
            .lines
            .iter()
            .any(|l| l.batch_no.is_some() || !l.serial_nos.is_empty());
        let mut columns = vec![
            Column::new("#"),
            Column::new("SKU"),
            Column::wide("Description"),
            Column::number("Accepted"),
        ];
        if rejected {
            columns.push(Column::number("Rejected"));
            columns.push(Column::new("Reason"));
        }
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
                ];
                if rejected {
                    cells.push(quantity(l.rejected_qty));
                    cells.push(l.reject_reason.clone().unwrap_or_default());
                }
                if traced {
                    cells.push(trace_of(l.batch_no.as_deref(), &l.serial_nos));
                }
                cells
            })
            .collect();

        Ok(Document {
            title: "Goods Received Note".to_string(),
            number: r.number.clone(),
            status: r.status.as_str().replace('_', " "),
            party_label: "Supplier",
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
                Signature::new("Received by").dated(),
                Signature::new("Checked by").dated(),
            ],
        }
        .into_report())
    }
}
