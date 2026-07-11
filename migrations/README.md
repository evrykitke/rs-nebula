# Module migrations

Business modules migrate their schema with **pure SQL files** kept here,
one folder per module:

```
migrations/
  sales/
    invoices_0001.sql
    invoices_0002.sql
    customers_0001.sql
```

The framework applies every `.sql` file it finds under `migrations/<module>/`
in **filename order**, on the main database and on every tenant database
(at boot, when a tenant database is provisioned, and via the tenant
migration job). Each file is recorded in `nebula_sql_migrations` and runs
**once per database**.

The framework's own ("system", in-house) schema is *not* here — it is
owned by the SeaORM migrator in `crates/nebula/src/migrations.rs`. Only
application modules migrate through this folder.

## Conventions

- **One migration per file.** Name it `<table>_<number>.sql`, zero-padding
  the number so lexical order is apply order (`invoices_0001.sql`,
  `invoices_0002.sql`).
- **Index as you create.** Every foreign key and every column you filter,
  join or order on should have an index in the same file — a read should
  never fall back to a sequential scan. For example:

  ```sql
  CREATE TABLE invoices (
      id           UUID PRIMARY KEY,
      tenant_id    UUID NOT NULL,
      customer_id  UUID NOT NULL,
      number       TEXT NOT NULL,
      status       TEXT NOT NULL,
      issued_at    TIMESTAMPTZ NOT NULL,
      total_minor  BIGINT NOT NULL
  );

  CREATE INDEX ix_invoices_customer ON invoices (customer_id);
  CREATE INDEX ix_invoices_status_issued ON invoices (status, issued_at);
  CREATE UNIQUE INDEX ux_invoices_number ON invoices (number);
  ```

- **A file runs inside a transaction**, so `CREATE INDEX CONCURRENTLY`
  (which cannot) is not supported — use a plain `CREATE INDEX`.
- **Never edit an applied file.** It has already run everywhere; add a new
  numbered file for the change.

The folder scanned is `migrations.root` in configuration (default
`migrations`).
