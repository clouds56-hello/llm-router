# Persistence schemas

This directory is the authoritative source for SQLite schema files used by the
`persistence` crate. Reorganizing files here must not change the effective DB
schema unless a migration is intentionally added.

## Layout

- `snapshot/<db>/vX.Y.Z.sql`: versioned full-schema snapshots.
- `migrations/<db>/000N_name.sql`: forward-only migrations after the initial
  released schema.
- `squash/<db>/vA.B.C_vX.Y.Z_000N_000M.sql`: maintenance-only rollups from one
  snapshot version to another.

## Version mapping

- `snapshot/<db>/v0.0.0.sql` is the original released schema and still maps to
  migration version `1` in Rust.
- `snapshot/<db>/v0.1.1.sql` is the latest equivalent schema for fresh
  databases and replaces the old `000_bootstrap.sql` files.
- `squash/<db>/...` files are derived artifacts for review and maintenance, not
  runtime inputs.

## Rules

- Do not edit old migration files after release.
- Keep each snapshot schema equivalent to applying all migrations for that DB.
- Keep each squash file equivalent to the sequence it replaces.
- Restructure-only changes in this directory must not change SQL semantics.
