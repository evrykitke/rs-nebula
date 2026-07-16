//! The delivery note that travels with the goods and is signed for on
//! arrival.

use super::{Addressed, party_block, party_of, status_line};
use crate::scm::document::{Document, date, quantity};
use crate::scm::sales::delivery::{DeliveryService, DeliveryView};
use crate::scm::sales::permissions::names;
use nebula::{
    Column, DataCx, Error, KeyValue, Report, ReportData, ReportDataSource, ReportDefinition,
    ReportOutput, Result, Signature,
};
use std::sync::Arc;

const KEY: &str = "scm_delivery_note_doc";

pub struct DeliveryDataSource;

#[async_trait::async_trait]
impl ReportDataSource for DeliveryDataSource {
    fn key(&self) -> &'static str {
        KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let record = DeliveryService::new(db.clone()).view(cx.params.id()?).await?;
        let party = party_of(cx, record.customer_id, &record.customer_name).await?;
        serde_json::to_value(Addressed { record, party })
            .map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct DeliveryNoteDocument;

impl ReportDefinition for DeliveryNoteDocument {
    fn name(&self) -> &'static str {
        "delivery-note"
    }
    fn title(&self) -> &'static str {
        "Delivery Note"
    }
    fn group(&self) -> &'static str {
        "Sales"
    }
    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf]
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::DELIVERIES_VIEW)
    }
    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(DeliveryDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let Addressed { record: d, party } = data.get::<Addressed<DeliveryView>>(KEY)?;

        let mut meta = vec![KeyValue::new("Delivery date", date(d.delivery_date))];
        if let Some(o) = d.order_number.as_deref().filter(|s| !s.trim().is_empty()) {
            meta.push(KeyValue::new("Order", o));
        }
        for (label, value) in [
            ("Carrier", d.carrier.as_deref()),
            ("Tracking", d.tracking_no.as_deref()),
            ("Vehicle", d.vehicle_reg.as_deref()),
            ("Driver", d.driver_name.as_deref()),
        ] {
            if let Some(v) = value.filter(|s| !s.trim().is_empty()) {
                meta.push(KeyValue::new(label, v));
            }
        }

        let ship_to = match d.shipping_address.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(a) => a.lines().map(|l| l.trim().to_string()).collect(),
            None => party.shipping.clone(),
        };

        // A delivery note carries no prices: it proves what arrived, and the
        // person signing for it at the door has no business seeing the
        // margin.
        let traced = d
            .lines
            .iter()
            .any(|l| l.batch_no.is_some() || !l.serial_nos.is_empty());
        let mut columns = vec![
            Column::new("#"),
            Column::new("SKU"),
            Column::wide("Description"),
            Column::number("Qty"),
        ];
        if traced {
            columns.push(Column::new("Batch / serials"));
        }

        let rows = d
            .lines
            .iter()
            .map(|l| {
                let mut cells = vec![
                    l.line_no.to_string(),
                    l.sku.clone(),
                    l.item_name.clone(),
                    quantity(l.qty),
                ];
                if traced {
                    let mut trace = l.batch_no.clone().unwrap_or_default();
                    if !l.serial_nos.is_empty() {
                        if !trace.is_empty() {
                            trace.push(' ');
                        }
                        trace.push_str(&l.serial_nos.join(", "));
                    }
                    cells.push(trace);
                }
                cells
            })
            .collect();

        Ok(Document {
            title: "Delivery Note".to_string(),
            number: d.number.clone(),
            status: status_line(d.status.as_str(), None),
            party_label: "Customer",
            party: party_block(&party, &party.billing),
            second_label: Some("Deliver to"),
            second: ship_to,
            meta,
            columns,
            rows,
            totals: Vec::new(),
            terms: None,
            memo: d.memo.clone(),
            signatures: vec![
                Signature::new("Delivered by").dated(),
                // Printing the expected recipient's name where it is known
                // gives the driver someone to ask for.
                match d.received_by_name.as_deref().filter(|s| !s.trim().is_empty()) {
                    Some(name) => Signature::new("Received by").name(name).dated(),
                    None => Signature::new("Received by").dated(),
                },
            ],
        }
        .into_report())
    }
}
