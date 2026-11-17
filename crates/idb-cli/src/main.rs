#![allow(non_snake_case)]
//! db-rs: i-db 의 Rust 판. 지금은 화면 없는 하위 명령(R58)만 옮기는 중이다.

mod board_cmd;
mod cli;
mod command;
mod conn_cmd;
mod flows_cmd;
mod goflag;
mod history_cmd;
mod mcp;
mod output;
mod source_cmd;
mod status_cmd;
mod update_cmd;
mod usage_text;

use std::io::{IsTerminal, Write};

/// 화면 플래그는 `-p 폴더`·`--remote 이름`(`=` 꼴과 대시 하나도)뿐이다.
fn tui_options(args: &[String]) -> Result<idb_tui::Options, String> {
    let mut o = idb_tui::Options {
        project: None,
        remote: None,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (a.as_str(), None),
        };
        let slot = match flag {
            "-p" | "--p" => &mut o.project,
            "-remote" | "--remote" => &mut o.remote,
            _ => return Err(format!("db-rs: unknown flag {a} (db-rs [-p 폴더] [--remote 이름])")),
        };
        let v = match inline {
            Some(v) => v,
            None => it.next().cloned().ok_or_else(|| format!("db-rs: flag needs an argument: {flag}"))?,
        };
        if v.is_empty() {
            return Err(format!("db-rs: flag needs an argument: {flag}"));
        }
        *slot = Some(v);
    }
    Ok(o)
}

/// 앞에 둔 `-p 폴더`·`--remote 이름` 뒤에 하위 명령이 오면(`db-rs --remote devbox conns`, 별칭 `dbw conns`)
/// 그 플래그를 명령 뒤로 옮긴다. 하위 명령이 없으면 그대로 두어 화면 플래그로 읽힌다.
fn lead_flags_after_command(args: Vec<String>) -> Vec<String> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let name = a.split_once('=').map_or(a, |(n, _)| n);
        if !matches!(name, "-p" | "--p" | "-remote" | "--remote") {
            break;
        }
        i += if a.contains('=') { 1 } else { 2 };
    }
    let is_command = |a: &str| a == "serve" || a == "mcp" || cli::is_cli_command(a);
    match args.get(i) {
        Some(cmd) if i > 0 && is_command(cmd) => {
            // 명령 바로 뒤에 넣어 뒤에서 다시 준 같은 플래그가 이기게 한다(Go flag 는 마지막 값을 쓴다).
            // 단, `conn` 은 첫 자리가 하위 명령(add·test…)이라 그 뒤에 넣는다
            let rest = &args[i + 1..];
            let sub = usize::from(cmd == "conn" && rest.first().is_some_and(|a| !a.starts_with('-')));
            let mut out = vec![cmd.clone()];
            out.extend_from_slice(&rest[..sub]);
            out.extend_from_slice(&args[..i]);
            out.extend_from_slice(&rest[sub..]);
            out
        }
        _ => args,
    }
}

fn main() {
    let args = lead_flags_after_command(std::env::args().skip(1).collect());
    if args.first().is_some_and(|a| a == "serve") {
        // 패닉 값에는 SQL·서버 응답이 섞여 비밀번호가 나올 수 있어 찍지 않는다
        std::panic::set_hook(Box::new(|_| {}));
        std::process::exit(cli::run_serve(&args[1..]));
    }
    // 하위 명령이 없으면 화면이다: `db-rs`, `db-rs -p 폴더`, `db-rs --remote 이름`
    if args.first().is_none_or(|a| a.starts_with('-')) {
        std::process::exit(match tui_options(&args) {
            Ok(o) => idb_tui::run(o),
            Err(e) => {
                eprintln!("{e}");
                cli::EXIT_USAGE
            }
        });
    }
    if args.first().is_some_and(|a| a == "mcp") {
        std::panic::set_hook(Box::new(|_| {}));
        std::process::exit(mcp::run_mcp(&args[1..]));
    }
    if args.first().is_none_or(|a| !cli::is_cli_command(a)) {
        eprintln!("db-rs: unknown command {:?} (db-rs help)", args[0]);
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

#[cfg(test)]
mod lead_flag_tests {
    use super::lead_flags_after_command;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn 앞에_둔_remote_는_하위_명령_뒤로_옮긴다() {
        assert_eq!(
            lead_flags_after_command(v(&["--remote", "devbox", "conns"])),
            v(&["conns", "--remote", "devbox"])
        );
        assert_eq!(
            lead_flags_after_command(v(&["--remote=devbox", "-p", "x", "query", "-c", "a"])),
            v(&["query", "--remote=devbox", "-p", "x", "-c", "a"])
        );
        assert_eq!(
            lead_flags_after_command(v(&["-p", "x", "conn", "test", "-h"])),
            v(&["conn", "test", "-p", "x", "-h"])
        );
        assert_eq!(
            lead_flags_after_command(v(&["--remote", "w", "redis", "-c", "r", "--", "LRANGE", "k", "0", "-1"])),
            v(&["redis", "--remote", "w", "-c", "r", "--", "LRANGE", "k", "0", "-1"])
        );
        assert_eq!(
            lead_flags_after_command(v(&["--remote", "devbox", "mcp"])),
            v(&["mcp", "--remote", "devbox"])
        );
    }

    #[test]
    fn 뒤에서_다시_준_플래그가_앞_것을_이긴다() {
        // 앞 것은 명령 바로 뒤에 두고 뒤 것은 그대로 남아, 파서가 마지막 값을 쓴다
        assert_eq!(
            lead_flags_after_command(v(&["-p", "/A", "conns", "-p", "/B"])),
            v(&["conns", "-p", "/A", "-p", "/B"])
        );
        // 다른 플래그의 값으로 온 `-p` 는 건드리지 않는다
        assert_eq!(
            lead_flags_after_command(v(&["-p", "/A", "linked", "--value", "-p"])),
            v(&["linked", "-p", "/A", "--value", "-p"])
        );
    }

    #[test]
    fn 하위_명령이_없으면_화면_플래그로_둔다() {
        assert_eq!(lead_flags_after_command(v(&["--remote", "devbox"])), v(&["--remote", "devbox"]));
        assert_eq!(lead_flags_after_command(v(&["conns", "-p", "x"])), v(&["conns", "-p", "x"]));
        assert_eq!(lead_flags_after_command(v(&["-p", "x", "nope"])), v(&["-p", "x", "nope"]));
    }
}
