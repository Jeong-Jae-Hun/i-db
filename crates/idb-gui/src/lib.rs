#![allow(non_snake_case)]
//! i-db 의 macOS GUI. 화면은 `ui`(GPUI, macOS 전용)가 그리고, 무엇을 보일지는 `vm` 이 정한다.
//! 데이터는 모두 `idb_engine::Engine` 으로 가서 로컬 프로젝트든 ssh 너머 `db serve` 든 같은 길로 돈다.
//! 안전장치(읽기 전용, prod 확인·잠금, 위험 문장)는 엔진이 정하고 화면은 그 reason 을 확인 창으로 바꿀 뿐이다.

pub mod vm;

#[cfg(target_os = "macos")]
pub mod ui;
