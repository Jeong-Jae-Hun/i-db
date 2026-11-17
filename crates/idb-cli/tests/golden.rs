#![allow(non_snake_case)]
//! 골든 기대 출력(testdata/golden/out)과 db 출력을 바이트로 비교한다.
//! 형식·자리 규칙은 tests/common/mod.rs 머리 주석에 있다. rust-ported.txt 에 적은 케이스만 통과를 요구하고,
//! 적지 않은 케이스가 이미 맞으면 옮겨 적으라고 알린다.

mod common;

#[test]
fn 골든_케이스의_출력이_Go_판과_바이트로_같다() {
    common::run_golden(None);
}
