# rs-nebula

A Domain-Driven Design application framework for building ERPs in Rust,
inspired by [ASP.NET Boilerplate](https://aspnetboilerplate.com/).
Applications are composed from modules, bootstrapped by a kernel, and
configured rather than hardcoded.

```rust
use nebula::Kernel;

#[tokio::main]
async fn main() -> nebula::Result<()> {
    Kernel::builder().build()?.run().await
}
```

Every application gets, with zero code: layered yaml configuration with
validation at boot, tracing-based logging, SeaORM database connectivity
with migrations and readiness checks, a generic repository and unit of
work, toggleable multitenancy (database-per-tenant with a directory
database), currency-safe `Money`, OpenAPI + Swagger UI, and resilient
web defaults (timeouts, panic containment, request ids, RFC 9457
problem+json errors).

## Documentation

Full documentation lives in the
[rs-nebula-docs](https://github.com/evrykitke/rs-nebula-docs) repository:
setup, architecture, dataflow and roadmap.

## Quick start

You need a reachable PostgreSQL server; point the framework at it via
`dev.local.yaml` (gitignored) or `NEBULA__DATABASE__URL`.

```sh
docker compose up -d        # optional: Redis + RabbitMQ
cargo run -p nebula-server
```

Then open <http://127.0.0.1:5000/swagger-ui>.

## Workspace

| Crate | Role |
|---|---|
| `nebula` | The framework library |
| `nebula-server` | Host binary bootstrapped by the kernel |
| `nebula-tests` | Proof-of-concept test suite |
