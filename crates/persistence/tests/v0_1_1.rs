use llm_persistence::migrate::{self, Bootstrap, Migration};
use rusqlite::Connection;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

const FIXTURE_JSON: &str = include_str!("fixtures/v0.1.1.json");

const REQUESTS_V0_0_0: &str = include_str!("../schemas/snapshot/requests/v0.0.0.sql");
const REQUESTS_V0_1_1: &str = include_str!("../schemas/snapshot/requests/v0.1.1.sql");
const REQUESTS_SQUASH_V0_1_1: &str = include_str!("../schemas/squash/requests/v0.0.0_v0.1.1_0001_0006.sql");
const REQUESTS_MIGRATIONS: &[Migration] = &[
  Migration {
    version: 1,
    name: "initial",
    sql: REQUESTS_V0_0_0,
  },
  Migration {
    version: 2,
    name: "add_correlation_and_error",
    sql: include_str!("../schemas/migrations/requests/0002_add_correlation_and_error.sql"),
  },
  Migration {
    version: 3,
    name: "add_usage_breakdown",
    sql: include_str!("../schemas/migrations/requests/0003_add_usage_breakdown.sql"),
  },
  Migration {
    version: 4,
    name: "add_response_header_latency",
    sql: include_str!("../schemas/migrations/requests/0004_add_response_header_latency.sql"),
  },
  Migration {
    version: 5,
    name: "add_source_and_method",
    sql: include_str!("../schemas/migrations/requests/0005_add_source_and_method.sql"),
  },
  Migration {
    version: 6,
    name: "add_context_and_metrics",
    sql: include_str!("../schemas/migrations/requests/0006_add_context_and_metrics.sql"),
  },
];

const SESSIONS_V0_0_0: &str = include_str!("../schemas/snapshot/sessions/v0.0.0.sql");
const SESSIONS_V0_1_1: &str = include_str!("../schemas/snapshot/sessions/v0.1.1.sql");
const SESSIONS_SQUASH_V0_1_1: &str = include_str!("../schemas/squash/sessions/v0.0.0_v0.1.1_0001_0001.sql");
const SESSIONS_MIGRATIONS: &[Migration] = &[Migration {
  version: 1,
  name: "initial",
  sql: SESSIONS_V0_0_0,
}];

const USAGE_V0_0_0: &str = include_str!("../schemas/snapshot/usage/v0.0.0.sql");
const USAGE_V0_1_1: &str = include_str!("../schemas/snapshot/usage/v0.1.1.sql");
const USAGE_SQUASH_V0_1_1: &str = include_str!("../schemas/squash/usage/v0.0.0_v0.1.1_0001_0004.sql");
const USAGE_MIGRATIONS: &[Migration] = &[
  Migration {
    version: 1,
    name: "initial",
    sql: USAGE_V0_0_0,
  },
  Migration {
    version: 2,
    name: "add_correlation_ids",
    sql: include_str!("../schemas/migrations/usage/0002_add_correlation_ids.sql"),
  },
  Migration {
    version: 3,
    name: "lifecycle_columns",
    sql: include_str!("../schemas/migrations/usage/0003_lifecycle_columns.sql"),
  },
  Migration {
    version: 4,
    name: "add_usage_breakdown",
    sql: include_str!("../schemas/migrations/usage/0004_add_usage_breakdown.sql"),
  },
];

#[derive(Clone, Copy)]
struct DbCase {
  name: &'static str,
  v0_0_0: &'static str,
  v0_1_1: &'static str,
  squash_v0_1_1: &'static str,
  migrations: &'static [Migration],
}

const REQUESTS_CASE: DbCase = DbCase {
  name: "requests",
  v0_0_0: REQUESTS_V0_0_0,
  v0_1_1: REQUESTS_V0_1_1,
  squash_v0_1_1: REQUESTS_SQUASH_V0_1_1,
  migrations: REQUESTS_MIGRATIONS,
};

const SESSIONS_CASE: DbCase = DbCase {
  name: "sessions",
  v0_0_0: SESSIONS_V0_0_0,
  v0_1_1: SESSIONS_V0_1_1,
  squash_v0_1_1: SESSIONS_SQUASH_V0_1_1,
  migrations: SESSIONS_MIGRATIONS,
};

const USAGE_CASE: DbCase = DbCase {
  name: "usage",
  v0_0_0: USAGE_V0_0_0,
  v0_1_1: USAGE_V0_1_1,
  squash_v0_1_1: USAGE_SQUASH_V0_1_1,
  migrations: USAGE_MIGRATIONS,
};

#[test]
fn requests_v0_1_1_migrations_and_squash_match_fixture() {
  assert_case(REQUESTS_CASE);
}

#[test]
fn sessions_v0_1_1_migrations_and_squash_match_fixture() {
  assert_case(SESSIONS_CASE);
}

#[test]
fn usage_v0_1_1_migrations_and_squash_match_fixture() {
  assert_case(USAGE_CASE);
}

fn assert_case(case: DbCase) {
  let fixture = fixture_case(case.name);
  let dir = tempdir(case.name);

  let incremental_path = dir.join("incremental.db");
  seed_v0_db(&incremental_path, case, fixture);
  let incremental_state = run_incremental(&incremental_path, case);

  let squash_path = dir.join("squash.db");
  seed_v0_db(&squash_path, case, fixture);
  let squash_state = run_squash(&squash_path, case);

  let expected = fixture.get("expected_rows").and_then(Value::as_object).expect("expected_rows object");
  assert_eq!(incremental_state.version, fixture_expected_version(fixture));
  assert_eq!(squash_state.version, fixture_expected_version(fixture));
  assert_expected_tables(&incremental_state.tables, expected);
  assert_expected_tables(&squash_state.tables, expected);
  assert_eq!(incremental_state.tables, squash_state.tables, "{} migrated and squashed results diverged", case.name);
}

fn run_incremental(path: &Path, case: DbCase) -> DbState {
  let mut conn = Connection::open(path).unwrap();
  migrate::apply(&mut conn, path, case.name, Bootstrap { sql: case.v0_1_1 }, case.migrations).unwrap();
  dump_state(&conn)
}

fn run_squash(path: &Path, case: DbCase) -> DbState {
  let conn = Connection::open(path).unwrap();
  conn.execute_batch(case.squash_v0_1_1).unwrap();
  mark_versions(&conn, case.migrations);
  dump_state(&conn)
}

fn seed_v0_db(path: &Path, case: DbCase, fixture: &Value) {
  let conn = Connection::open(path).unwrap();
  ensure_schema_table(&conn);
  conn.execute_batch(case.v0_0_0).unwrap();
  for sql in fixture
    .get("seed_sql")
    .and_then(Value::as_array)
    .expect("seed_sql array")
  {
    conn.execute_batch(sql.as_str().expect("seed SQL string")).unwrap();
  }
  conn
    .execute(
      "INSERT INTO schema_migrations (version, name, applied_ts) VALUES (1, ?1, 0)",
      [case.migrations[0].name],
    )
    .unwrap();
}

fn ensure_schema_table(conn: &Connection) {
  conn
    .execute_batch(
      "CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, applied_ts INTEGER NOT NULL);",
    )
    .unwrap();
}

fn mark_versions(conn: &Connection, migrations: &[Migration]) {
  for migration in migrations.iter().skip(1) {
    conn
      .execute(
        "INSERT INTO schema_migrations (version, name, applied_ts) VALUES (?1, ?2, 0)",
        rusqlite::params![migration.version, migration.name],
      )
      .unwrap();
  }
}

#[derive(Debug, PartialEq)]
struct DbState {
  version: u32,
  tables: Map<String, Value>,
}

fn dump_state(conn: &Connection) -> DbState {
  let version = migrate::read_current_version(conn).unwrap();
  let table_names = conn
    .prepare(
      "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name <> 'schema_migrations' ORDER BY name",
    )
    .unwrap()
    .query_map([], |row| row.get::<_, String>(0))
    .unwrap()
    .collect::<rusqlite::Result<Vec<_>>>()
    .unwrap();
  let mut tables = Map::new();
  for table in table_names {
    tables.insert(table.clone(), Value::Array(read_table_rows(conn, &table)));
  }
  DbState { version, tables }
}

fn read_table_rows(conn: &Connection, table: &str) -> Vec<Value> {
  let sql = format!("SELECT rowid, * FROM \"{}\" ORDER BY rowid", table.replace('"', "\"\""));
  let mut stmt = conn.prepare(&sql).unwrap();
  let col_names: Vec<String> = (1..stmt.column_count())
    .map(|i| stmt.column_name(i).unwrap_or("").to_string())
    .collect();
  let rows = stmt
    .query_map([], |row| {
      let mut out = Map::new();
      for (offset, name) in col_names.iter().enumerate() {
        let value = row.get_ref(offset + 1)?;
        out.insert(name.clone(), value_to_json(value));
      }
      Ok(Value::Object(out))
    })
    .unwrap();
  rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
}

fn value_to_json(value: rusqlite::types::ValueRef<'_>) -> Value {
  match value {
    rusqlite::types::ValueRef::Null => Value::Null,
    rusqlite::types::ValueRef::Integer(n) => Value::Number(n.into()),
    rusqlite::types::ValueRef::Real(f) => serde_json::Number::from_f64(f)
      .map(Value::Number)
      .unwrap_or(Value::Null),
    rusqlite::types::ValueRef::Text(bytes) => match std::str::from_utf8(bytes) {
      Ok(s) => serde_json::from_str::<Value>(s).unwrap_or_else(|_| Value::String(s.to_string())),
      Err(_) => Value::Array(bytes.iter().map(|b| Value::from(*b)).collect()),
    },
    rusqlite::types::ValueRef::Blob(bytes) => match std::str::from_utf8(bytes) {
      Ok(s) => serde_json::from_str::<Value>(s).unwrap_or_else(|_| Value::String(s.to_string())),
      Err(_) => Value::Array(bytes.iter().map(|b| Value::from(*b)).collect()),
    },
  }
}

fn assert_expected_tables(actual_tables: &Map<String, Value>, expected_tables: &Map<String, Value>) {
  for (table, expected_rows) in expected_tables {
    let actual_rows = actual_tables.get(table).unwrap_or_else(|| panic!("missing table {table}"));
    let expected_rows = expected_rows.as_array().expect("expected row array");
    let actual_rows = actual_rows.as_array().expect("actual row array");
    assert_eq!(actual_rows.len(), expected_rows.len(), "row count mismatch for {table}");
    for (actual, expected) in actual_rows.iter().zip(expected_rows) {
      assert_expected_row(table, actual, expected);
    }
  }
}

fn assert_expected_row(table: &str, actual: &Value, expected: &Value) {
  let actual = actual.as_object().expect("actual row object");
  let expected = expected.as_object().expect("expected row object");
  for (key, expected_value) in expected {
    let actual_value = actual.get(key).unwrap_or_else(|| panic!("missing column {table}.{key}"));
    assert_eq!(actual_value, expected_value, "value mismatch for {table}.{key}");
  }
}

fn fixture_case(name: &str) -> &'static Value {
  static FIXTURE: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
  FIXTURE
    .get_or_init(|| serde_json::from_str(FIXTURE_JSON).expect("valid fixture JSON"))
    .get(name)
    .unwrap_or_else(|| panic!("missing fixture case {name}"))
}

fn fixture_expected_version(fixture: &Value) -> u32 {
  fixture
    .get("expected_version")
    .and_then(Value::as_u64)
    .expect("expected_version") as u32
}

fn tempdir(name: &str) -> PathBuf {
  let path = std::env::temp_dir().join(format!("llm-router-schema-test-{name}-{}", uuid::Uuid::new_v4()));
  std::fs::create_dir_all(&path).unwrap();
  path
}
