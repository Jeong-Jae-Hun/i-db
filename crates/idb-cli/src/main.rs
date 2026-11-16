//! db-rs: i-db 의 Rust 판. 지금은 화면 없는 하위 명령(R58)만 옮기는 중이다.

mod cli;
mod goflag;
mod output;
mod usage_text;

use std::io::{IsTerminal, Write};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_none_or(|a| !cli::is_cli_command(a)) {
        eprintln!("db-rs: only CLI commands are ported yet (db-rs help)");
        std::process::exit(cli::EXIT_USAGE);
    }
    let stdin_tty = std::io::stdin().is_terminal();
    let mut stdin = std::io::stdin();
    let (mut out, mut err) = (String::new(), String::new());
    let code = cli::run_cli(
        &args,
        if stdin_tty { None } else { Some(&mut stdin) },
        &mut out,
        &mut err,
        std::io::stdout().is_terminal(),
    );
    let _ = std::io::stdout().write_all(out.as_bytes());
    let _ = std::io::stderr().write_all(err.as_bytes());
    std::process::exit(code);
}
