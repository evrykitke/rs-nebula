//! Workspace reports. `WorkspaceOverview` is a tour of every widget the
//! engine offers; `SampleRegister` is a list report that also exports to
//! Excel. Both use only the framework company datasource (resolved
//! automatically), so they need no business data yet.

use nebula::{
    Callout, CalloutStyle, Chart, ChartKind, Column, Group, KeyValue, Metric, Progress, Report,
    ReportData, ReportDefinition, ReportFormat, ReportOutput, Result, Series, Signature, SpaceSize,
    Symbology, Table, TextStyle, Trend, Widget,
};

/// A single report that exercises the full widget set — headings, styled
/// text, KPI tiles, a chart, side-by-side blocks, a boxed table group, a
/// progress bar, a callout, lists, a QR/barcode pair and a signature band.
pub struct WorkspaceOverview;

impl ReportDefinition for WorkspaceOverview {
    fn name(&self) -> &'static str {
        "workspace-overview"
    }

    fn title(&self) -> &'static str {
        "Workspace Overview"
    }

    fn default_format(&self) -> ReportFormat {
        ReportFormat::Modern
    }

    fn build(&self, _data: &ReportData) -> Result<Report> {
        let bill_to = Widget::KeyValues {
            title: Some("Bill to".into()),
            items: vec![
                KeyValue::new("Customer", "Acme Trading Ltd"),
                KeyValue::new("Address", "P.O. Box 1234, Nairobi"),
                KeyValue::new("PIN", "P051234567X"),
            ],
            columns: 1,
        };
        let meta = Widget::KeyValues {
            title: Some("Document".into()),
            items: vec![
                KeyValue::new("Number", "INV-2026-00042"),
                KeyValue::new("Date", "12 Jul 2026"),
                KeyValue::new("Due", "26 Jul 2026"),
            ],
            columns: 1,
        };

        let lines = Table::new(vec![
            Column::new("Item"),
            Column::center("Qty"),
            Column::number("Unit"),
            Column::number("Amount"),
        ])
        .row(["Consulting — August", "10", "3,000.00", "30,000.00"])
        .row(["Support retainer", "1", "12,000.00", "12,000.00"])
        .row(["Onboarding", "2", "8,500.00", "17,000.00"])
        .totals(["", "", "Total", "59,000.00"]);

        let revenue = Chart {
            kind: ChartKind::Bar,
            title: Some("Monthly revenue (KES '000)".into()),
            labels: vec![
                "Mar".into(),
                "Apr".into(),
                "May".into(),
                "Jun".into(),
                "Jul".into(),
            ],
            series: vec![Series {
                name: "2026".into(),
                values: vec![320.0, 410.0, 380.0, 505.0, 590.0],
            }],
        };

        Ok(Report::new("Workspace Overview")
            .subtitle("A tour of the reporting engine's widgets")
            .with(Widget::heading(1, "Summary"))
            .with(Widget::styled(
                "Every element below is a widget composed into one report. \
                 Switching the format re-skins all of it.",
                TextStyle::Muted,
            ))
            .with(Widget::metrics(vec![
                Metric::new("Revenue", "KES 4.2M")
                    .caption("+12% vs last month")
                    .trend(Trend::Up),
                Metric::new("Orders", "1,284")
                    .caption("+3.1%")
                    .trend(Trend::Up),
                Metric::new("Avg. order", "KES 3,270")
                    .caption("flat")
                    .trend(Trend::Flat),
            ]))
            .with(Widget::Chart(revenue))
            .with(Widget::Divider)
            .with(Widget::heading(2, "Document blocks"))
            .with(Widget::columns(vec![vec![bill_to], vec![meta]]))
            .with(
                Group::new(vec![lines.into_widget()])
                    .title("Invoice lines")
                    .boxed()
                    .into_widget(),
            )
            .with(Widget::spacer(SpaceSize::Small))
            .with(Progress::new("Quarterly target", 0.68).caption("68% of KES 20M").into_widget())
            .with(
                Callout::new(
                    CalloutStyle::Warning,
                    "All figures on this page are illustrative sample data.",
                )
                .title("Note")
                .into_widget(),
            )
            .with(Widget::heading(2, "What the engine gives you"))
            .with(Widget::bullets(vec![
                "Composable widgets — table, chart, metrics, callouts, more",
                "Swappable formats — Modern, Compact, Corporate",
                "PDF for any report; Excel for list reports",
            ]))
            .with(Widget::columns(vec![
                vec![Widget::QrCode {
                    data: "https://example.com/pay/INV-2026-00042".into(),
                    caption: Some("Scan to pay".into()),
                }],
                vec![Widget::Barcode {
                    data: "INV-2026-00042".into(),
                    symbology: Symbology::Code128,
                    caption: Some("Document number".into()),
                }],
            ]))
            .with(Widget::Signatures {
                items: vec![
                    Signature::new("Prepared by").dated(),
                    Signature::new("Approved by").dated(),
                ],
            }))
    }
}

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

    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf, ReportOutput::Excel]
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
        .row(["01 Jul", "INV-2026-00039", "Acme Trading Ltd", "30,000.00", "Paid"])
        .row(["04 Jul", "INV-2026-00040", "Blue Ridge Co.", "12,500.00", "Paid"])
        .row(["09 Jul", "INV-2026-00041", "Cedar Holdings", "48,200.00", "Overdue"])
        .row(["12 Jul", "INV-2026-00042", "Acme Trading Ltd", "59,000.00", "Sent"])
        .totals(["", "", "Total", "149,700.00", ""]);

        Ok(Report::new("Sample Register")
            .subtitle("A list report — also available as Excel")
            .with(table.into_widget()))
    }
}
