//! Outbound mail.
//!
//! Each tenant configures its own SMTP server, so one deployment sends as
//! many different companies — an invoice from Acme must arrive from Acme's
//! own mail server, not the host's. There is deliberately no deployment
//! fallback relay: mail that silently went out under the wrong identity
//! would be worse than mail that did not go out at all.
//!
//! - [`Mailer`] — reads a tenant's settings, builds a transport, sends
//! - [`MailSettings`] — the client-safe view: says *whether* a password is
//!   set, never what it is
//! - [`Message`] — what to send, with optional attachments
//!
//! The SMTP password is encrypted at rest ([`crate::crypto`]) because,
//! unlike a user password, it has to be replayed to the mail server and so
//! cannot be hashed. It is write-only across the API.

pub mod settings;

use crate::config::{Config, Secret};
use crate::crypto::Cipher;
use crate::error::{Error, Result};
use chrono::Utc;
use lettre::message::header::ContentType;
use lettre::message::{Attachment as LettreAttachment, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message as LettreMessage, Tokio1Executor};
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, IntoActiveModel, Set};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use uuid::Uuid;

/// How the connection to the SMTP server is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Encryption {
    /// Cleartext. Only for a relay on the same host or private network:
    /// the password crosses the wire in the clear.
    None,
    /// Connect in the clear, then upgrade. The usual choice on port 587.
    StartTls,
    /// TLS from the first byte. The usual choice on port 465.
    Tls,
}

impl Encryption {
    fn as_str(self) -> &'static str {
        match self {
            Encryption::None => "none",
            Encryption::StartTls => "starttls",
            Encryption::Tls => "tls",
        }
    }

    /// Unknown values read as `StartTls` rather than `None`: a column we
    /// cannot parse must not silently downgrade a tenant to cleartext.
    fn parse(value: &str) -> Self {
        match value {
            "none" => Encryption::None,
            "tls" => Encryption::Tls,
            _ => Encryption::StartTls,
        }
    }
}

/// A tenant's mail configuration as the API reports it. The password is
/// absent by construction — [`MailSettings::password_set`] says whether
/// there is one, which is all a settings page needs to know.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MailSettings {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password_set: bool,
    pub encryption: Encryption,
    pub from_address: String,
    pub from_name: Option<String>,
    pub enabled: bool,
    pub updated_at: chrono::DateTime<Utc>,
}

/// What an admin submits. `password: None` leaves whatever is stored
/// alone, so a settings form can round-trip without ever holding the
/// password; `Some("")` clears it.
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
pub struct MailSettingsInput {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub encryption: Encryption,
    pub from_address: String,
    pub from_name: Option<String>,
    pub enabled: bool,
}

/// A file to send along with a message.
pub struct Attachment {
    pub file_name: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// An email to send. `html` is optional: with it the message goes out as
/// multipart with `text` as the fallback every client can read.
pub struct Message {
    pub to: String,
    pub subject: String,
    pub text: String,
    pub html: Option<String>,
    pub attachments: Vec<Attachment>,
}

impl Message {
    pub fn new(to: impl Into<String>, subject: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            to: to.into(),
            subject: subject.into(),
            text: text.into(),
            html: None,
            attachments: Vec::new(),
        }
    }

    pub fn html(mut self, html: impl Into<String>) -> Self {
        self.html = Some(html.into());
        self
    }

    pub fn attach(mut self, attachment: Attachment) -> Self {
        self.attachments.push(attachment);
        self
    }
}

/// Sends mail on behalf of tenants. Cheap to clone; the transport is built
/// per send from the tenant's current settings, so a settings change takes
/// effect on the next message rather than the next restart.
#[derive(Clone)]
pub struct Mailer {
    main_db: DatabaseConnection,
    /// Built on demand rather than at boot: a deployment whose tenants
    /// never configure mail should not be made to invent an encryption key.
    encryption_key: Secret,
    timeout: Duration,
}

impl Mailer {
    pub fn new(main_db: DatabaseConnection, config: &Config) -> Self {
        Self {
            main_db,
            encryption_key: config.security.encryption_key.clone(),
            timeout: Duration::from_secs(config.mail.timeout_secs),
        }
    }

    fn cipher(&self) -> Result<Cipher> {
        Cipher::new(&self.encryption_key)
    }

    /// A tenant's settings, or `None` when mail was never configured.
    pub async fn settings(&self, tenant_id: Uuid) -> Result<Option<MailSettings>> {
        Ok(self.row(tenant_id).await?.map(into_view))
    }

    async fn row(&self, tenant_id: Uuid) -> Result<Option<settings::Model>> {
        Ok(settings::Entity::find_by_id(tenant_id)
            .one(&self.main_db)
            .await?)
    }

    /// Store a tenant's settings. Encrypting the password here is what
    /// forces `security.encryption_key` to exist — a deployment learns it
    /// needs one the first time a tenant configures mail, not at boot.
    pub async fn save(&self, tenant_id: Uuid, input: MailSettingsInput) -> Result<MailSettings> {
        validate(&input)?;
        let existing = self.row(tenant_id).await?;

        // None keeps what is stored; Some("") clears it; anything else
        // replaces it.
        let password_encrypted = match input.password.as_deref() {
            None => existing.as_ref().and_then(|e| e.password_encrypted.clone()),
            Some("") => None,
            Some(password) => Some(self.cipher()?.encrypt(password)?),
        };

        let mut active = match existing {
            Some(row) => row.into_active_model(),
            None => settings::ActiveModel {
                tenant_id: Set(tenant_id),
                ..Default::default()
            },
        };
        active.host = Set(input.host.trim().to_string());
        active.port = Set(input.port as i32);
        active.username = Set(input.username.filter(|u| !u.trim().is_empty()));
        active.password_encrypted = Set(password_encrypted);
        active.encryption = Set(input.encryption.as_str().to_string());
        active.from_address = Set(input.from_address.trim().to_string());
        active.from_name = Set(input.from_name.filter(|n| !n.trim().is_empty()));
        active.enabled = Set(input.enabled);
        active.updated_at = Set(Utc::now());

        let saved = match self.row(tenant_id).await? {
            Some(_) => active.update(&self.main_db).await?,
            None => active.insert(&self.main_db).await?,
        };
        Ok(into_view(saved))
    }

    /// Send a message as a tenant. Fails when mail is unconfigured or
    /// switched off — callers that treat mail as optional should check
    /// [`Mailer::settings`] first rather than swallow this.
    pub async fn send(&self, tenant_id: Uuid, message: Message) -> Result<()> {
        let Some(row) = self.row(tenant_id).await? else {
            return Err(Error::Validation(
                "this company has not configured a mail server".into(),
            ));
        };
        if !row.enabled {
            return Err(Error::Validation(
                "outbound mail is switched off for this company".into(),
            ));
        }
        self.dispatch(&row, message).await
    }

    /// Send using settings that may not be stored yet, so an admin can
    /// prove a server works before committing to it — and, on failure,
    /// gets the SMTP server's own complaint rather than a generic one.
    pub async fn send_test(
        &self,
        tenant_id: Uuid,
        input: &MailSettingsInput,
        to: &str,
    ) -> Result<()> {
        validate(input)?;
        let stored = self.row(tenant_id).await?;
        // The form may withhold the password because it never held it;
        // fall back to the stored one so "test" tests what is configured.
        let password_encrypted = match input.password.as_deref() {
            None => stored.as_ref().and_then(|s| s.password_encrypted.clone()),
            Some("") => None,
            Some(password) => Some(self.cipher()?.encrypt(password)?),
        };
        let row = settings::Model {
            tenant_id,
            host: input.host.trim().to_string(),
            port: input.port as i32,
            username: input.username.clone().filter(|u| !u.trim().is_empty()),
            password_encrypted,
            encryption: input.encryption.as_str().to_string(),
            from_address: input.from_address.trim().to_string(),
            from_name: input.from_name.clone(),
            enabled: true,
            updated_at: Utc::now(),
        };
        let message = Message::new(
            to,
            "Test message",
            "This is a test message from your ERP. If you are reading it, \
             your mail settings work.",
        );
        self.dispatch(&row, message).await
    }

    async fn dispatch(&self, row: &settings::Model, message: Message) -> Result<()> {
        let email = build_email(row, message)?;
        let transport = self.transport(row)?;
        transport
            .send(email)
            .await
            .map_err(|e| Error::Validation(format!("the mail server refused the message: {e}")))?;
        Ok(())
    }

    fn transport(&self, row: &settings::Model) -> Result<AsyncSmtpTransport<Tokio1Executor>> {
        let host = row.host.as_str();
        let mut builder = match Encryption::parse(&row.encryption) {
            Encryption::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(host)
                .map_err(|e| Error::Validation(format!("could not reach {host}: {e}")))?,
            Encryption::StartTls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host)
                .map_err(|e| Error::Validation(format!("could not reach {host}: {e}")))?,
            Encryption::None => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host),
        };
        builder = builder.port(row.port as u16).timeout(Some(self.timeout));

        if let Some(username) = &row.username {
            let password = match &row.password_encrypted {
                Some(stored) => self.cipher()?.decrypt(stored)?,
                None => Secret::default(),
            };
            builder = builder.credentials(Credentials::new(
                username.clone(),
                password.expose().to_string(),
            ));
        }
        Ok(builder.build())
    }
}

fn build_email(row: &settings::Model, message: Message) -> Result<LettreMessage> {
    let from = match &row.from_name {
        Some(name) => format!("{name} <{}>", row.from_address),
        None => row.from_address.clone(),
    };
    let builder = LettreMessage::builder()
        .from(
            from.parse()
                .map_err(|e| Error::Validation(format!("invalid sender address {from:?}: {e}")))?,
        )
        .to(message.to.parse().map_err(|e| {
            Error::Validation(format!("invalid recipient {:?}: {e}", message.to))
        })?)
        .subject(message.subject);

    let text = SinglePart::plain(message.text);
    // A message with no attachments and no HTML is a plain body; anything
    // richer has to be assembled as multipart.
    if message.attachments.is_empty() && message.html.is_none() {
        return builder
            .singlepart(text)
            .map_err(|e| Error::internal(format!("could not build the message: {e}")));
    }

    let body = match message.html {
        Some(html) => MultiPart::alternative()
            .singlepart(text)
            .singlepart(SinglePart::html(html)),
        None => MultiPart::mixed().singlepart(text),
    };
    let mut mixed = MultiPart::mixed().multipart(body);
    for attachment in message.attachments {
        let content_type = ContentType::parse(&attachment.content_type).map_err(|e| {
            Error::Validation(format!(
                "invalid attachment content type {:?}: {e}",
                attachment.content_type
            ))
        })?;
        mixed = mixed.singlepart(
            LettreAttachment::new(attachment.file_name).body(attachment.bytes, content_type),
        );
    }
    builder
        .multipart(mixed)
        .map_err(|e| Error::internal(format!("could not build the message: {e}")))
}

fn into_view(row: settings::Model) -> MailSettings {
    MailSettings {
        host: row.host,
        port: row.port as u16,
        username: row.username,
        password_set: row.password_encrypted.is_some(),
        encryption: Encryption::parse(&row.encryption),
        from_address: row.from_address,
        from_name: row.from_name,
        enabled: row.enabled,
        updated_at: row.updated_at,
    }
}

fn validate(input: &MailSettingsInput) -> Result<()> {
    if input.host.trim().is_empty() {
        return Err(Error::Validation("the mail server host is required".into()));
    }
    if input.port == 0 {
        return Err(Error::Validation(
            "the mail server port must be between 1 and 65535".into(),
        ));
    }
    if !is_address(input.from_address.trim()) {
        return Err(Error::Validation(format!(
            "{:?} is not a valid sender address",
            input.from_address
        )));
    }
    Ok(())
}

fn is_address(value: &str) -> bool {
    value.len() <= 255
        && value.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && domain.contains('.') && !domain.starts_with('.')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> MailSettingsInput {
        MailSettingsInput {
            host: "smtp.example.com".into(),
            port: 587,
            username: Some("postmaster@example.com".into()),
            password: Some("hunter2".into()),
            encryption: Encryption::StartTls,
            from_address: "billing@example.com".into(),
            from_name: Some("Acme Billing".into()),
            enabled: true,
        }
    }

    fn row() -> settings::Model {
        settings::Model {
            tenant_id: Uuid::new_v4(),
            host: "smtp.example.com".into(),
            port: 587,
            username: None,
            password_encrypted: None,
            encryption: "starttls".into(),
            from_address: "billing@example.com".into(),
            from_name: Some("Acme Billing".into()),
            enabled: true,
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn a_sender_address_must_look_like_one() {
        let mut bad = input();
        bad.from_address = "not-an-address".into();
        assert!(validate(&bad).is_err());
        assert!(validate(&input()).is_ok());
    }

    #[test]
    fn a_host_is_required() {
        let mut bad = input();
        bad.host = "   ".into();
        assert!(validate(&bad).is_err());
    }

    /// An encryption value we cannot parse must fail safe. Reading it as
    /// `none` would put the tenant's password on the wire in the clear.
    #[test]
    fn an_unknown_encryption_does_not_downgrade_to_cleartext() {
        assert_eq!(Encryption::parse("nonsense"), Encryption::StartTls);
        assert_eq!(Encryption::parse(""), Encryption::StartTls);
        assert_eq!(Encryption::parse("none"), Encryption::None);
    }

    #[test]
    fn encryption_round_trips_through_the_column() {
        for e in [Encryption::None, Encryption::StartTls, Encryption::Tls] {
            assert_eq!(Encryption::parse(e.as_str()), e);
        }
    }

    /// The whole point of the client-safe view: a settings response says
    /// whether a password exists and nothing more.
    #[test]
    fn the_view_reports_the_password_without_revealing_it() {
        let mut row = row();
        assert!(!into_view(row.clone()).password_set);
        row.password_encrypted = Some("deadbeef".into());
        let view = into_view(row);
        assert!(view.password_set);
        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("deadbeef"), "{json}");
    }

    #[test]
    fn a_named_sender_becomes_a_display_name() {
        let email = build_email(&row(), Message::new("to@example.com", "Hi", "Hello")).unwrap();
        let formatted = String::from_utf8(email.formatted()).unwrap();
        // lettre quotes the display name, per RFC 5322.
        assert!(
            formatted.contains(r#""Acme Billing" <billing@example.com>"#),
            "{formatted}"
        );
    }

    #[test]
    fn an_attachment_makes_the_message_multipart() {
        let message = Message::new("to@example.com", "Invoice", "See attached").attach(Attachment {
            file_name: "sinv-2026-00003.pdf".into(),
            content_type: "application/pdf".into(),
            bytes: b"%PDF-1.7".to_vec(),
        });
        let email = build_email(&row(), message).unwrap();
        let formatted = String::from_utf8_lossy(&email.formatted()).to_string();
        assert!(formatted.contains("multipart/mixed"), "{formatted}");
        assert!(formatted.contains("sinv-2026-00003.pdf"), "{formatted}");
    }

    #[test]
    fn a_bad_recipient_is_refused_before_any_connection() {
        let message = Message::new("not-an-address", "Hi", "Hello");
        assert!(build_email(&row(), message).is_err());
    }
}
