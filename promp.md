Here is the corrected and polished version of your text. I fixed the spelling and grammar while keeping all your technical specifications and file paths exactly as you intended.

For better readability, I have broken the original block into logical paragraphs:

---

I want to create a Rust framework that I will eventually use to build an ERP, but I want to make the foundation as strong as possible. My inspiration is to implement Domain-Driven Design (DDD) in Rust. One framework I admire is ASP.NET Boilerplate ([https://aspnetboilerplate.com/](https://aspnetboilerplate.com/)); therefore, this framework will largely borrow concepts from it.

We will use the following tools: PostgreSQL for the database, Redis for caching, Apalis for background jobs, and RabbitMQ. Redis and RabbitMQ will run in Docker. Please create a `docker-compose.yml` file to set those up. Configure our Rust target to output to `D:\Ak\Dev\Ruru\Bin`, and our frontend will be built with Angular in `D:\Ak\Dev\Ruru\Pylon-frontend`. Axum and Utoipa will be used. Service proxies will be generated using NSwag, and RxJS will be used for interactivity.

We're going to start small and build incrementally. `Kernel.rs` will bootstrap everything and be used in `main.rs`. Also, come up with a test project to write our unit tests. Unit tests are not mandatory but, when needed, will be used as a proof of concept. Note that time zones, date formatting, and numeric formatting for money will be critical; we do not want bugs due to rounding errors.

From the onset, approach the solution with a "what could go wrong here?" mentality. This will help us envision problems that are likely to occur. Important areas to consider include security, efficiency, non-blocking operations, and mitigating against deadlocks. Jobs are our best buddies when it comes to long-running processes. Events are also a good way for internal systems to communicate with each other. Our API should conform to the OpenAPI specification.

The framework should support multitenancy (where each tenant has its own database) alongside a main database to act as a directory for the app. The application can also be configured to disable multitenancy. This is particularly important for self-hosted apps that only need a single database to run (e.g., SaaS for self-hosted customers).

Configuration over hardcoding and composition are key concepts we want to ingrain. We need to create a framework that makes the developer an artist rather than a plumber (ensuring a good developer experience). Documentation is key. We will have a docs folder in `D:\Ak\Dev\Ruru\PylonDocs`. I prefer documentation to be separated into multiple files (e.g., setup, architecture, dataflow) instead of one single file.

Finally, tooling is important for common, repetitive tasks. We can build a tool for that, but this will come later after we are established. Here are the framework defaults that should be implemented: Authentication and authorization. Authorization will follow the ASP.NET Zero permission system (e.g., `Pages.Administration`, `Pages.Administration.Users.Edit`, `Pages.Sales.Invoices.Post`, `Pages.Sales.Quotes.View`). Users will have roles, and roles will have permissions, but these permissions can be overridden on a per-user basis.

---

### A Quick Structural Breakdown

If you are planning to use this as a prompt or a project specification document, here is the exact same text organized into a scannable format:

**Core Architecture & Philosophy**

* **Goal:** Build a robust, Domain-Driven Design (DDD) Rust framework as the foundation for an ERP.
* **Inspiration:** ASP.NET Boilerplate concepts.
* **Design Principles:** Configuration over hardcoding, composition, developer experience ("artist rather than a plumber"), and a defensive "what could go wrong?" mentality.
* **Key Focus Areas:** Security, efficiency, non-blocking operations, deadlock mitigation, background jobs for long-running processes, and event-driven internal communication.
* **Precision Requirements:** Strict handling of time zones, date formatting, and financial numeric formatting to prevent rounding errors.

**Tech Stack & Tooling**

* **Backend:** Rust (Axum, Utoipa, Apalis for jobs).
* **Database & Messaging:** PostgreSQL (primary DB), Redis (caching), RabbitMQ (events). Both Redis and RabbitMQ to run via Docker (`docker-compose.yml`).
* **Frontend:** Angular, utilizing NSwag for service proxies and RxJS for interactivity.
* **API:** Must conform to the OpenAPI specification.

**Project Structure & Paths**

* **Rust Target Binaries:** `D:\Ak\Dev\Ruru\Bin`
* **Frontend Directory:** `D:\Ak\Dev\Ruru\Pylon-frontend`
* **Documentation:** `D:\Ak\Dev\Ruru\PylonDocs` (split into separate files like setup, architecture, and dataflow).
* **Entry Point:** `Kernel.rs` will bootstrap the application and be consumed by `main.rs`.
* **Testing:** A dedicated test project for unit testing (used primarily as proofs of concept).

**Features & Modules**

* **Multitenancy:** Database-per-tenant architecture with a central directory database. Must be toggleable for single-tenant, self-hosted customers.
* **Authentication & Authorization:** Role-based access control (RBAC) mirroring the ASP.NET Zero permission system (e.g., `Pages.Administration`, `Pages.Sales.Invoices.Post`). Permissions must be overridable on a per-user basis.
* ** Audit logs with entity snapshots (before and after)