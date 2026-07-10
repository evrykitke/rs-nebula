//! The currency table, stored in the main database. Pre-populated by a
//! framework migration with the world's currencies (`is_system: true`,
//! undeletable); deployments add their own units through
//! [`super::CurrencyModule`]'s endpoints. Tenants pick their default
//! currency from this list.

use crate::error::{Error, Result};
use sea_orm::entity::prelude::*;
use sea_orm::{DatabaseConnection, QueryOrder, Set};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, utoipa::ToSchema)]
#[sea_orm(table_name = "currencies")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    /// ISO 4217 code (or an app-defined unit), three uppercase letters.
    #[sea_orm(unique)]
    pub code: String,
    pub name: String,
    /// Decimal places of the minor unit (KES/USD 2, JPY 0, BHD 3).
    pub minor_units: i16,
    /// Seeded rows are system currencies and cannot be deleted.
    pub is_system: bool,
    #[schema(value_type = String, format = DateTime)]
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// Data access for the currency table (always the main database).
pub struct Store {
    db: DatabaseConnection,
}

pub struct NewCurrency {
    pub code: String,
    pub name: String,
    pub minor_units: i16,
}

impl Store {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn find_all(&self) -> Result<Vec<Model>> {
        Entity::find()
            .order_by_asc(Column::Code)
            .all(&self.db)
            .await
            .map_err(Error::from)
    }

    pub async fn find_by_code(&self, code: &str) -> Result<Option<Model>> {
        Entity::find()
            .filter(Column::Code.eq(code))
            .one(&self.db)
            .await
            .map_err(Error::from)
    }

    pub async fn create(&self, new: NewCurrency) -> Result<Model> {
        if !(0..=6).contains(&new.minor_units) {
            return Err(Error::Validation(
                "minor_units must be between 0 and 6".into(),
            ));
        }
        // Same code shape rules as the Money type, so every row is usable.
        super::Currency::new(&new.code, new.minor_units as u8)?;
        if new.name.trim().is_empty() {
            return Err(Error::Validation("currency name must not be empty".into()));
        }
        if self.find_by_code(&new.code).await?.is_some() {
            return Err(Error::Conflict(format!(
                "currency {:?} already exists",
                new.code
            )));
        }
        ActiveModel {
            id: Set(Uuid::new_v4()),
            code: Set(new.code),
            name: Set(new.name.trim().to_string()),
            minor_units: Set(new.minor_units),
            is_system: Set(false),
            created_at: Set(chrono::Utc::now()),
        }
        .insert(&self.db)
        .await
        .map_err(Error::from)
    }

    /// Delete a deployment-added currency. System rows are reference
    /// data every tenant relies on and cannot be removed.
    pub async fn delete(&self, id: Uuid) -> Result<Model> {
        let row = Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .ok_or_else(|| Error::NotFound(format!("currency {id}")))?;
        if row.is_system {
            return Err(Error::Validation(format!(
                "{} is a system currency and cannot be deleted",
                row.code
            )));
        }
        Entity::delete_by_id(id).exec(&self.db).await?;
        Ok(row)
    }
}
