//! 모든 복사 경로(결과 표·값 보기·편집기)가 쓰는 클립보드 쓰기다(R71.4). Go `internal/clipboard` 와 같다.
//! OSC 52 로 터미널 클립보드에 쓰고, 시스템 클립보드 명령(pbcopy·clip.exe·wl-copy·xclip)이 있으면 그쪽에도 쓴다.
//! OSC 52 를 막은 터미널이 있어서다. DB_CLIPBOARD=osc52·system·both(기본)로 고른다.

use std::io::Write;
use std::time::{Duration, Instant};

/// WSL 기본 마운트 자리의 cmd.exe 다(automount root 를 바꿨으면 PATH 에 cmd.exe 가 있어야 한다)
pub const WSL_CMD: &str = "/mnt/c/Windows/System32/cmd.exe";
const WSL_MARKER: &str = "/proc/sys/fs/binfmt_misc/WSLInterop";
const RUN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Osc52,
    System,
    Both,
}

type Getenv = Box<dyn Fn(&str) -> Option<String> + Send>;
type Runner = Box<dyn Fn(&str, &[String], &[u8], Duration) -> Result<(), String> + Send>;

/// 바깥(OS·환경 변수·PATH·명령 실행)이다. 시험이 바꿔 끼워 실제 클립보드 명령을 부르지 않는다.
pub struct Sys {
    pub os: &'static str,
    pub getenv: Getenv,
    pub on_path: Box<dyn Fn(&str) -> bool + Send>,
    pub exists: Box<dyn Fn(&str) -> bool + Send>,
    /// 명령에 입력을 넣고 timeout 안에 끝나길 기다린다. 성공이 아니면 까닭이다
    pub run: Runner,
}

impl Sys {
    pub fn real() -> Sys {
        Sys {
            os: std::env::consts::OS,
            getenv: Box::new(|k| std::env::var(k).ok()),
            on_path: Box::new(on_path),
            exists: Box::new(|p| std::path::Path::new(p).exists()),
            run: Box::new(run_command),
        }
    }

    /// DB_CLIPBOARD 값이다. 모르는 값은 기본(둘 다)이다.
    pub fn mode(&self) -> Mode {
        match (self.getenv)("DB_CLIPBOARD").unwrap_or_default().trim().to_lowercase().as_str() {
            "osc52" => Mode::Osc52,
            "system" => Mode::System,
            _ => Mode::Both,
        }
    }

    /// 이 OS 의 클립보드 명령이다. WSL 은 리눅스지만 Windows 클립보드(clip.exe)로 보낸다.
    /// clip.exe 는 stdin 을 콘솔 코드 페이지로 읽어 UTF-8 로 바꾼 셸에서 부른다.
    pub fn system_command(&self) -> Option<(String, Vec<String>)> {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let clip = s(&["/c", "chcp 65001 >nul && clip"]);
        let wsl = (self.getenv)("WSL_DISTRO_NAME").is_some_and(|v| !v.is_empty()) || (self.exists)(WSL_MARKER);
        let cands: Vec<(String, Vec<String>)> = if self.os == "macos" || self.os == "darwin" {
            vec![("pbcopy".into(), vec![])]
        } else if self.os == "windows" || wsl {
            vec![("cmd.exe".into(), clip.clone()), (WSL_CMD.into(), clip)]
        } else if (self.getenv)("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty()) {
            vec![("wl-copy".into(), vec![]), ("xclip".into(), s(&["-selection", "clipboard"]))]
        } else {
            vec![("xclip".into(), s(&["-selection", "clipboard"])), ("wl-copy".into(), vec![])]
        };
        cands.into_iter().find(|(c, _)| (self.on_path)(c))
    }

    /// 시스템 클립보드 명령만 돌린 결과다(명령을 기다리므로 화면 밖 스레드에서 부른다).
    pub fn system_write(&self, s: &str) -> Result<(), String> {
        match self.system_command() {
            Some((name, args)) => (self.run)(&name, &args, s.as_bytes(), RUN_TIMEOUT),
            None => Err("no system clipboard command".into()),
        }
    }
}

/// 모드와 두 쪽 결과로 복사의 성패를 정한다. 둘 다에서는 하나라도 되면 성공이고(시스템 명령 실패는 OSC 52 가 됐으면
/// 조용히 넘긴다), system 모드는 명령 결과 그대로다. osc 는 OSC 52 를 쓴 결과(osc52 모드가 아니면 None 일 수 있음)다.
pub fn outcome(mode: Mode, osc: Option<Result<(), String>>, sys: Option<Result<(), String>>) -> Result<(), String> {
    let osc = osc.unwrap_or(Ok(()));
    match mode {
        Mode::Osc52 => osc,
        Mode::System => sys.unwrap_or(Ok(())),
        Mode::Both => match (osc, sys.unwrap_or(Ok(()))) {
            (Err(e), Err(_)) => Err(e),
            _ => Ok(()),
        },
    }
}

/// OSC 52 로 쓴다. 화면 그리기와 섞이지 않게 화면 스레드에서 부른다.
pub fn write_osc52(out: &mut dyn Write, s: &str) -> Result<(), String> {
    write!(out, "\x1b]52;c;{}\x07", base64(s.as_bytes()))
        .and_then(|()| out.flush())
        .map_err(|e| e.to_string())
}

fn on_path(name: &str) -> bool {
    if name.contains('/') {
        return std::path::Path::new(name).is_file();
    }
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
}

/// 명령에 in 을 넣고 timeout 안에 끝나길 기다린다. stdout·stderr 는 잇지 않는다(xclip·wl-copy 는 선택을 들고 있으려
/// 자식을 남기는데 파이프를 이으면 그 자식이 끝날 때까지 막혀서다). 성공 종료가 아니면 오류다.
fn run_command(name: &str, args: &[String], input: &[u8], timeout: Duration) -> Result<(), String> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(name)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input);
    }
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => return Ok(()),
            Ok(Some(st)) => return Err(st.to_string()),
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                return Err("signal: killed (2s timeout)".into());
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn base64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    type Calls = Arc<Mutex<Vec<(String, Vec<String>, Vec<u8>)>>>;

    fn fake(os: &'static str, env: &[(&str, &str)], path: &[&str], wsl: bool, fail: Option<&str>) -> (Sys, Calls) {
        let env: Vec<(String, String)> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let path: Vec<String> = path.iter().map(|s| s.to_string()).collect();
        let calls: Calls = Arc::default();
        let c = calls.clone();
        let fail = fail.map(String::from);
        let sys = Sys {
            os,
            getenv: Box::new(move |k| env.iter().find(|(x, _)| x == k).map(|(_, v)| v.clone())),
            on_path: Box::new(move |n| path.iter().any(|p| p == n)),
            exists: Box::new(move |p| wsl && p == WSL_MARKER),
            run: Box::new(move |n, a, i, t| {
                assert!(t > Duration::ZERO, "시간 제한 없이 명령을 불렀다");
                c.lock().unwrap().push((n.into(), a.to_vec(), i.to_vec()));
                fail.clone().map_or(Ok(()), Err)
            }),
        };
        (sys, calls)
    }

    #[test]
    fn OS마다_클립보드_명령을_고른다() {
        let all = ["pbcopy", "cmd.exe", WSL_CMD, "wl-copy", "xclip"];
        type Case<'a> = (&'a str, &'static str, &'a [(&'a str, &'a str)], &'a [&'a str], bool, &'a str);
        let cases: &[Case] = &[
            ("맥은 pbcopy", "macos", &[], &all, false, "pbcopy"),
            (
                "WSL 환경 변수면 cmd.exe",
                "linux",
                &[("WSL_DISTRO_NAME", "Ubuntu")],
                &all,
                false,
                "cmd.exe",
            ),
            ("WSLInterop 파일이면 cmd.exe", "linux", &[], &all, true, "cmd.exe"),
            ("Windows 는 cmd.exe", "windows", &[], &all, false, "cmd.exe"),
            (
                "WSL PATH 에 Windows 가 없으면 /mnt/c",
                "linux",
                &[],
                &[WSL_CMD, "xclip"],
                true,
                WSL_CMD,
            ),
            (
                "Wayland 면 wl-copy",
                "linux",
                &[("WAYLAND_DISPLAY", "wayland-0")],
                &all,
                false,
                "wl-copy",
            ),
            ("X 는 xclip", "linux", &[], &all, false, "xclip"),
            ("xclip 이 없으면 wl-copy", "linux", &[], &["wl-copy"], false, "wl-copy"),
            ("명령이 없으면 없음", "linux", &[], &[], false, ""),
            ("WSL 에 cmd.exe 가 없으면 없음", "linux", &[], &["xclip"], true, ""),
        ];
        for (name, os, env, path, wsl, want) in cases {
            let (sys, _) = fake(os, env, path, *wsl, None);
            let got = sys.system_command();
            assert_eq!(got.as_ref().map_or("", |g| g.0.as_str()), *want, "{name}");
            if let Some((n, a)) = got
                && n == "xclip"
            {
                assert_eq!(a.join(" "), "-selection clipboard");
            }
        }
    }

    #[test]
    fn 모드는_DB_CLIPBOARD_로_고르고_모르는_값은_둘_다다() {
        for (v, want) in [
            ("", Mode::Both),
            ("osc52", Mode::Osc52),
            ("SYSTEM", Mode::System),
            ("both", Mode::Both),
            ("x", Mode::Both),
        ] {
            let (sys, _) = fake("linux", &[("DB_CLIPBOARD", v)], &[], false, None);
            assert_eq!(sys.mode(), want, "{v:?}");
        }
    }

    #[test]
    fn WSL_은_코드_페이지를_UTF8로_바꾼_clip에_보낸다() {
        let (sys, calls) = fake("linux", &[("WSL_DISTRO_NAME", "Ubuntu")], &[WSL_CMD], false, None);
        sys.system_write("가").unwrap();
        let c = calls.lock().unwrap();
        assert_eq!(
            (c[0].0.as_str(), c[0].1.join(" "), c[0].2.as_slice()),
            (WSL_CMD, "/c chcp 65001 >nul && clip".into(), "가".as_bytes())
        );
    }

    #[test]
    fn system_모드에서_명령이_없거나_실패하면_오류다() {
        // 명령 없음
        let (sys, _) = fake("linux", &[("DB_CLIPBOARD", "system")], &[], false, None);
        let r = outcome(sys.mode(), None, Some(sys.system_write("a")));
        assert_eq!(r, Err("no system clipboard command".into()));
        // 명령이 실패(종료 코드)
        let (sys, calls) = fake("macos", &[("DB_CLIPBOARD", "system")], &["pbcopy"], false, Some("exit status: 1"));
        let r = outcome(sys.mode(), None, Some(sys.system_write("a")));
        assert_eq!((r, calls.lock().unwrap().len()), (Err("exit status: 1".into()), 1));
    }

    #[test]
    fn 둘_다일_때_한쪽만_실패하면_조용히_넘기고_둘_다_실패하면_오류다() {
        let (sys, _) = fake("macos", &[], &["pbcopy"], false, Some("exit 1"));
        assert_eq!(outcome(sys.mode(), Some(Ok(())), Some(sys.system_write("a"))), Ok(()));
        assert_eq!(outcome(Mode::Both, Some(Err("closed".into())), Some(Ok(()))), Ok(()));
        let (sys, _) = fake("linux", &[], &[], false, None);
        assert_eq!(
            outcome(sys.mode(), Some(Err("closed".into())), Some(sys.system_write("a"))),
            Err("closed".into())
        );
        let mut out = Vec::new();
        write_osc52(&mut out, "한글 a").unwrap();
        assert_eq!(out, b"\x1b]52;c;7ZWc6riAIGE=\x07");
    }

    #[test]
    fn 실제_명령의_실패_종료는_오류다() {
        assert!(run_command("false", &[], b"x", RUN_TIMEOUT).is_err());
        assert!(run_command("true", &[], b"x", RUN_TIMEOUT).is_ok());
        assert!(run_command("/no/such/cmd", &[], b"x", RUN_TIMEOUT).is_err());
    }
}
