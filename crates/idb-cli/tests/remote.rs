#![allow(non_snake_case)]
//! `--remote` 고리 시험: 골든 케이스를 `db serve` 를 거쳐 돌려 로컬(Go 판)과 바이트로 같은지 본다.
//! ssh 대신 DB_REMOTE_CMD(시험 전용)로 serve 를 바로 띄운다. live 케이스는 DBTUI_TEST_PG_URL·DBTUI_TEST_REDIS_ADDR 가 있을 때만 돈다.

mod common;

/// --remote 로 돌리는 이미 옮긴 명령들이다.
const REMOTE: &[&str] = &[
    "conns",
    "schemas",
    "tables",
    "describe",
    "ddl",
    "query",
    "explain",
    "search",
    "redis",
    "linked",
    "schema-diff",
    "user360",
    "logs",
    "metrics",
    "status",
    "flows",
    "board",
];

#[test]
fn 원격으로_돌린_골든_케이스도_Go_판과_바이트로_같다() {
    common::run_golden(Some(REMOTE));
}
