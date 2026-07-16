//! The sales order confirmation sent back to the customer.

use super::{Addressed, party_block, party_of, status_line};
use crate::scm::document::{Document, amount, date, quantity, total_line};
use crate::scm::sales::order::{OrderService, OrderView};
use crate::scm::sales::permissions::names;
use nebula::{
    Column, DataCx, Error, KeyValue, Report, ReportData, ReportDataSource, ReportDefinition,
    ReportOutput, Result,
};
use std::sync::Arc;

const KEY: &str = "scm_sales_order_doc";

pub struct OrderDataSource;

#[async_trait::async_trait]
impl ReportDataSource for OrderDataSource {
    fn key(&self) -> &'static str {
        KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let record = OrderService::new(db.clone()).view(cx.params.id()?).await?;
        let party = party_of(cx, record.customer_id, &record.customer_name).await?;
        serde_json::to_value(Addressed { record, party })
            .map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct SalesOrderDocument;

impl ReportDefinition for SalesOrderDocument {
    fn name(&self) -> &'static str {
        "sales-order"
    }
    fn title(&self) -> &'static str {
        "Sales Order"
    }
    fn group(&self) -> &'static str {
        "Sales"
    }
    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf]
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::ORDERS_VIEW)
    }
    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(OrderDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let Addressed { record: o, party } = data.get::<Addressed<OrderView>>(KEY)?;

        let mut meta = vec![
            KeyValue::new("Order date", date(o.order_date)),
            KeyValue::new("Currency", o.currency.clone()),
        ];
        if let Some(e) = o.expected_date {
            meta.push(KeyValue::new("Expected", date(e)));
        }
        if o.payment_terms_days > 0 {
            meta.push(KeyValue::new(
                "Payment terms",
                format!("{} days", o.payment_terms_days),
            ));
        }
        if let Some(po) = o.customer_po_no.as_deref().filter(|s| !s.trim().is_empty()) {
            meta.push(KeyValue::new("Your PO", po));
        }
        if let Some(i) = o.incoterms.as_deref().filter(|s| !s.trim().is_empty()) {
            meta.push(KeyValue::new("Incoterms", i));
        }

        // The order's own shipping address wins over the customer's default:
        // this order may be going somewhere else.
        let ship_to = match o
            .shipping_address
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            Some(a) => a.lines().map(|l| l.trim().to_string()).collect(),
            None if !party.shipping.is_empty() => party.shipping.clone(),
            None => vec![format!("Warehouse {}", o.warehouse_code)],
        };

        let discounted = o.lines.iter().any(|l| l.discount_pct.is_some());
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

        let rows = o
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

        let mut totals = vec![total_line("Subtotal", o.subtotal)];
        if let Some(pct) = o.discount_pct.filter(|d| !d.is_zero()) {
            totals.push(KeyValue::new("Discount", format!("{}%", amount(pct))));
        }
        if let Some(a) = o.discount_amount.filter(|d| !d.is_zero()) {
            totals.push(total_line("Discount", a));
        }
        if let Some(c) = o.other_charges.filter(|d| !d.is_zero()) {
            totals.push(total_line("Other charges", c));
        }
        totals.push(total_line(&format!("Total ({})", o.currency), o.total));

        Ok(Document {
            title: "Sales Order".to_string(),
            number: o.number.clone().into(),
            status: status_line(o.status.as_str(), o.cancel_reason.as_deref()),
            party_label: "Customer",
            party: party_block(&party, &party.billing),
            second_label: Some("Ship to"),
            second: ship_to,
            meta,
            columns,
            rows,
            totals,
            terms: o.terms_and_conditions.clone(),
            memo: o.memo.clone(),
            signatures: Vec::new(),
        }
        .into_report())
    }
}
