#![allow(non_snake_case)]
//! i-db 의 화면 없는 코어. CLI·MCP·TUI·GUI 가 같이 쓴다.

pub mod atomicfile;
pub mod connstore;
pub mod favorites;
pub mod flows;
pub mod gojson;
pub mod gostrconv;
pub mod history;
pub mod params;
pub mod paramstore;
pub mod project;
pub mod recent;
pub mod redact;
pub mod result;
pub mod session;
pub mod sqlsafe;
pub mod statcrypt;
pub mod textutil;
pub mod user360;
