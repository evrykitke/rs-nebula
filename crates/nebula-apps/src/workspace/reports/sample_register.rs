//! Sample Register: a list report — one table, also exported to Excel.
//!
//! The shape an "invoice register", "top products" or similar report takes.
//! Sample data, so it renders on a bare tenant.

use nebula::{Column, Report, ReportData, ReportDefinition, ReportOutput, Result, Table};

/// A list report: one table, exportable to Excel. This is the shape the
/// "invoice register", "top products" and similar reports take.
pub struct SampleRegister;

impl ReportDefinition for SampleRegister {
    fn name(&self) -> &'static str {
        "sample-register"
    }

    fn title(&self) -> &'static str {
        "Sample Register"
    }

    fn group(&self) -> &'static str {
        "Workspace"
    }

    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf, ReportOutput::Excel, ReportOutput::Table]
    }

    fn build(&self, _data: &ReportData) -> Result<Report> {
        let table = Table::new(vec![
            Column::new("Date"),
            Column::new("Number"),
            Column::new("Customer"),
            Column::number("Amount"),
            Column::center("Status"),
        ])
        .title("Invoice register — July 2026")
        .row([
            "01 Jul",
            "INV-2026-00039",
            "Acme Trading Ltd",
            "30,000.00",
            "Paid",
        ])
        .row([
            "04 Jul",
            "INV-2026-00040",
            "Blue Ridge Co.",
            "12,500.00",
            "Paid",
        ])
        .row([
            "09 Jul",
            "INV-2026-00041",
            "Cedar Holdings",
            "48,200.00",
            "Overdue",
        ])
        .row([
            "12 Jul",
            "INV-2026-00042",
            "Acme Trading Ltd",
            "59,000.00",
            "Sent",
        ])
        .totals(["", "", "Total", "149,700.00", ""]);

        Ok(Report::new("Sample Register")
            .subtitle("A list report — also available as Excel")
            .with(table.into_widget()))
    }
}
