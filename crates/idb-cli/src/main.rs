//! db-rs: i-db 의 Rust 판. 지금은 화면 없는 하위 명령(R58)만 옮기는 중이다.

mod cli;
mod command;
mod goflag;
mod output;
mod source_cmd;
mod usage_text;

use std::io::{IsTerminal, Write};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_none_or(|a| !cli::is_cli_command(a)) {
        eprintln!("db-rs: only CLI commands are ported yet (db-rs help)");
        std::process::exit(cli::EXIT_USAGE);
    }
    let stdin_tty = std::io::stdin().is_terminal();
    let stdin: Option<Box<dyn std::io::Read + Send>> = if stdin_tty { None } else { Some(Box::new(std::io::stdin())) };
    let (mut out, mut err) = (String::new(), String::new());
    // 패닉 값과 스택에는 SQL·서버 응답이 섞여 비밀번호가 나올 수 있어 찍지 않는다
    std::panic::set_hook(Box::new(|_| {}));
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        cli::run_cli(&args, stdin, &mut out, &mut err, std::io::stdout().is_terminal())
    }));
    let code = run.unwrap_or_else(|_| {
        err.push_str("internal error\n");
        cli::EXIT_ERR
    });
    // 출력이 막히면(디스크 꽉 참·닫힌 파이프) 성공으로 끝내지 않는다. 결과를 파일로 받는 쪽이 빈 파일을 성공으로 믿게 된다
    let mut stdout = std::io::stdout();
    let written = stdout.write_all(out.as_bytes()).and_then(|()| stdout.flush());
    let _ = std::io::stderr().write_all(err.as_bytes());
    if let Err(e) = written {
        eprintln!("write stdout: {e}");
        std::process::exit(cli::EXIT_ERR);
    }
    std::process::exit(code);
}
