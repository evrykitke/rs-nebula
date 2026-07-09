//! Generic repository over a SeaORM entity, in the spirit of ABP's
//! `IRepository<TEntity>`: the common data-access verbs without
//! hand-writing plumbing per entity.
//!
//! ```ignore
//! let todos: Repository<todo::Entity> = Repository::new(ctx.require_db());
//! let todo = todos.get_by_id(7).await?;          // -> Error::NotFound if absent
//! ```
//!
//! For multi-step atomic work use [`crate::db::transaction`] and SeaORM
//! operations on the transaction handle; an ambient, request-scoped unit
//! of work is planned alongside the auditing milestone.

use crate::error::{Error, Result};
use sea_orm::{
    ActiveModelBehavior, ActiveModelTrait, DatabaseConnection, EntityTrait,
    IntoActiveModel, PaginatorTrait, PrimaryKeyTrait,
};
use std::marker::PhantomData;

/// The common data-access verbs for one entity, backed by the shared
/// connection pool (cloning the pool handle is cheap).
pub struct Repository<E: EntityTrait> {
    db: DatabaseConnection,
    _entity: PhantomData<E>,
}

impl<E: EntityTrait> Repository<E> {
    pub fn new(db: DatabaseConnection) -> Self {
        Self {
            db,
            _entity: PhantomData,
        }
    }

    /// The underlying pool, for queries beyond the common verbs.
    pub fn db(&self) -> &DatabaseConnection {
        &self.db
    }

    pub async fn find_by_id(
        &self,
        id: impl Into<<E::PrimaryKey as PrimaryKeyTrait>::ValueType>,
    ) -> Result<Option<E::Model>> {
        E::find_by_id(id.into()).one(&self.db).await.map_err(Error::from)
    }

    /// Like [`Repository::find_by_id`] but absence is an error —
    /// [`Error::NotFound`], which surfaces as a 404.
    pub async fn get_by_id(
        &self,
        id: impl Into<<E::PrimaryKey as PrimaryKeyTrait>::ValueType>,
    ) -> Result<E::Model> {
        self.find_by_id(id)
            .await?
            .ok_or_else(|| Error::NotFound(E::default().table_name().to_string()))
    }

    pub async fn find_all(&self) -> Result<Vec<E::Model>> {
        E::find().all(&self.db).await.map_err(Error::from)
    }

    pub async fn count(&self) -> Result<u64>
    where
        E::Model: sea_orm::FromQueryResult + Send + Sync,
    {
        E::find().count(&self.db).await.map_err(Error::from)
    }

    pub async fn insert<A>(&self, model: A) -> Result<E::Model>
    where
        A: ActiveModelTrait<Entity = E> + ActiveModelBehavior + Send,
        E::Model: IntoActiveModel<A>,
    {
        model.insert(&self.db).await.map_err(Error::from)
    }

    pub async fn update<A>(&self, model: A) -> Result<E::Model>
    where
        A: ActiveModelTrait<Entity = E> + ActiveModelBehavior + Send,
        E::Model: IntoActiveModel<A>,
    {
        model.update(&self.db).await.map_err(Error::from)
    }

    /// Delete by primary key; deleting a missing row is
    /// [`Error::NotFound`] so callers can distinguish it.
    pub async fn delete_by_id(
        &self,
        id: impl Into<<E::PrimaryKey as PrimaryKeyTrait>::ValueType>,
    ) -> Result<()> {
        let result = E::delete_by_id(id.into())
            .exec(&self.db)
            .await
            .map_err(Error::from)?;
        if result.rows_affected == 0 {
            return Err(Error::NotFound(E::default().table_name().to_string()));
        }
        Ok(())
    }
}
