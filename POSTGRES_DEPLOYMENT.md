# PostgreSQL deployment (2026-09-21)

Set CACHE_BACKEND=postgres and IMAGE_CACHE_BACKEND=postgres. SQLite/Redis/S3 remain optional standalone backends.

Use `DATABASE_URL=postgresql://unfurl:PASSWORD@postgres:5432/backend`.
The shared database uses a dedicated `unfurl` schema and login role with no access to other service schemas. Set its role-specific search_path before applying migrations. Do not share `_sqlx_migrations` between services.

Apply this release only to a fresh PostgreSQL schema, import a consistent source snapshot, verify table counts and content hashes, and then start the application. Preserve signing keys, sessions, IDs and OAuth clients during migration. Do not point this release at a SQLite URL.

The deployment, import and encrypted R2 backup runbook lives in the private `jihuayu/personal-infra` repository. Older SQLite instructions in this repository describe the previous release only.

Tests require `TEST_DATABASE_URL` pointing to a disposable local PostgreSQL instance. Account, Atrium and gallery create isolated test schemas; never run these tests with production credentials.
