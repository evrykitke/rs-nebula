//! Proof of concept: the generic repository and the transaction helper
//! (unit of work) against a live database.

use nebula::config::DatabaseConfig;
use nebula::{Error, Repository, db};
use sea_orm::{ActiveModelTrait, ConnectionTrait, DatabaseConnection, Set};

// --- A sample entity, as an application would define it ---

// Each test gets its own table so parallel test threads cannot interfere.

mod todo {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "poc_todos")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub title: String,
        pub done: bool,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

mod uow_todo {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "poc_uow_todos")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub title: String,
        pub done: bool,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

async fn connect(table: &str) -> Option<DatabaseConnection> {
    let Ok(url) = std::env::var("NEBULA_TEST_DATABASE_URL") else {
        eprintln!("SKIPPED: set NEBULA_TEST_DATABASE_URL to run database tests");
        return None;
    };
    let db = db::connect(&DatabaseConfig {
        url: url.as_str().into(),
        ..DatabaseConfig::default()
    })
    .await
    .expect("must connect");

    db.execute_unprepared(&format!(
        "DROP TABLE IF EXISTS {table}; \
         CREATE TABLE {table} (\
            id SERIAL PRIMARY KEY, \
            title TEXT NOT NULL, \
            done BOOLEAN NOT NULL DEFAULT FALSE)",
    ))
    .await
    .expect("must create table");
    Some(db)
}

#[tokio::test]
async fn repository_covers_the_common_verbs() {
    let Some(db) = connect("poc_todos").await else {
        return;
    };
    let todos: Repository<todo::Entity> = Repository::new(db);

    // insert
    let created = todos
        .insert(todo::ActiveModel {
            title: Set("write the ERP".into()),
            done: Set(false),
            ..Default::default()
        })
        .await
        .expect("insert must work");
    assert_eq!(created.title, "write the ERP");

    // find / get
    let found = todos.find_by_id(created.id).await.expect("find must work");
    assert_eq!(found.as_ref().map(|m| m.id), Some(created.id));
    assert!(todos.find_by_id(999_999).await.unwrap().is_none());

    let err = todos.get_by_id(999_999).await.expect_err("get must fail");
    assert!(matches!(err, Error::NotFound(ref what) if what == "poc_todos"));

    // update
    let mut active: todo::ActiveModel = found.unwrap().into();
    active.done = Set(true);
    let updated = todos.update(active).await.expect("update must work");
    assert!(updated.done);

    // count / find_all / delete
    assert_eq!(todos.count().await.unwrap(), 1);
    assert_eq!(todos.find_all().await.unwrap().len(), 1);
    todos
        .delete_by_id(created.id)
        .await
        .expect("delete must work");
    assert_eq!(todos.count().await.unwrap(), 0);

    let err = todos
        .delete_by_id(created.id)
        .await
        .expect_err("second delete must fail");
    assert!(matches!(err, Error::NotFound(_)));
}

#[tokio::test]
async fn unit_of_work_commits_on_ok_and_rolls_back_on_err() {
    let Some(db) = connect("poc_uow_todos").await else {
        return;
    };

    // Ok(..) commits.
    db::transaction(&db, |txn| {
        Box::pin(async move {
            uow_todo::ActiveModel {
                title: Set("kept".into()),
                done: Set(false),
                ..Default::default()
            }
            .insert(txn)
            .await?;
            Ok(())
        })
    })
    .await
    .expect("committing transaction must succeed");

    // Err(..) rolls everything back, even earlier successful writes.
    let result: nebula::Result<()> = db::transaction(&db, |txn| {
        Box::pin(async move {
            uow_todo::ActiveModel {
                title: Set("rolled back".into()),
                done: Set(false),
                ..Default::default()
            }
            .insert(txn)
            .await?;
            Err(Error::Validation("business rule violated".into()))
        })
    })
    .await;
    assert!(result.is_err());

    let todos: Repository<uow_todo::Entity> = Repository::new(db);
    let all = todos.find_all().await.unwrap();
    assert_eq!(all.len(), 1, "only the committed row may exist");
    assert_eq!(all[0].title, "kept");
}
