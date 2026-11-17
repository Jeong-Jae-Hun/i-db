//! `db conn add|edit|rm|test|folder`(R59.4·R66.4). Go `conncli.go` 를 옮긴다.
//! db 전용 접속을 화면 없이 더하고 고치고 지우고 시험한다. 비밀번호는 --url·--password-stdin 으로만 받고 찍지 않는다.

use std::io::{BufRead, Read, Write};
use std::time::Duration;

use idb_core::connstore::{self, DATAGRIP_REFUSAL, StoreError, conn_warning};
use idb_core::project::{Conn, Kind, Project, SOURCE_DB, folder_of, go_atoi, group_of};
use idb_core::textutil::printable;
use idb_sources::source::{kind_label, test_conn};

use crate::cli::{EXIT_ERR, EXIT_OK, EXIT_REFUSED, EXIT_USAGE, PROJECT_USAGE, conn_label, default_project_dir, load_project};
use crate::goflag::{FlagDef, FlagSet, FlagType, ParseError, Parsed};
use crate::usage_text::CONN_USAGE;

/// --password-stdin 으로 받는 비밀번호 상한이다.
const CONN_MAX_PASSWORD: usize = 64 << 10;

/// 종료 코드를 정한 오류다.
enum ConnErr {
    Usage(String),
    Refused(String),
    Run(String),
}

impl ConnErr {
    fn code(&self) -> i32 {
        match self {
            ConnErr::Usage(_) => EXIT_USAGE,
            ConnErr::Refused(_) => EXIT_REFUSED,
            ConnErr::Run(_) => EXIT_ERR,
        }
    }

    fn text(&self) -> &str {
        match self {
            ConnErr::Usage(s) | ConnErr::Refused(s) | ConnErr::Run(s) => s,
        }
    }
}

fn usage(s: impl Into<String>) -> ConnErr {
    ConnErr::Usage(s.into())
}

impl From<StoreError> for ConnErr {
    fn from(e: StoreError) -> ConnErr {
        ConnErr::Run(e.msg)
    }
}

impl From<String> for ConnErr {
    fn from(e: String) -> ConnErr {
        ConnErr::Run(e)
    }
}

/// 명령줄 값이다. given 은 명령줄에 준 플래그다.
struct ConnOpts {
    p: Parsed,
    dir: String,
    timeout_ns: i64,
}

impl ConnOpts {
    fn s(&self, n: &str) -> String {
        self.p.strs.get(n).cloned().unwrap_or_default()
    }

    fn b(&self, n: &str) -> bool {
        self.p.bools.get(n).copied().unwrap_or(false)
    }

    fn given(&self, n: &str) -> bool {
        self.p.set.contains(n)
    }
}

fn flag_set(sub: &str) -> Option<FlagSet> {
    let mut defs = Vec::new();
    let mut add = |name: &'static str, ty: FlagType, usage: &str, default: Option<String>| {
        defs.push(FlagDef {
            name,
            usage: usage.into(),
            ty,
            default,
        })
    };
    add(
        "p",
        FlagType::Str,
        PROJECT_USAGE,
        Some(default_project_dir()).filter(|d| !d.is_empty()),
    );
    add("timeout", FlagType::Duration, "전체 시간 제한", Some("30s".into()));
    match sub {
        "add" | "edit" => {
            add(
                "url",
                FlagType::Str,
                "접속 URL (postgres://user:pw@host:5432/db, redis://:pw@host:6379/0)",
                None,
            );
            add("group", FlagType::Str, "그룹", None);
            add("password-stdin", FlagType::Bool, "비밀번호를 표준 입력에서 읽기", None);
            add("force", FlagType::Bool, "접속 시험이 실패해도 저장", None);
            add("prod", FlagType::Bool, "prod 접속 허용", None);
            add("user", FlagType::Str, "사용자", None);
            add("auth", FlagType::Str, "저장소 인증 basic|sigv4|token (없으면 비움)", None);
            add("region", FlagType::Str, "AWS 리전 (sigv4·DynamoDB·aws)", None);
            add("profile", FlagType::Str, "AWS 프로필 (비우면 표준 체인)", None);
            add("workgroup", FlagType::Str, "Athena 작업 그룹 (비우면 primary)", None);
            add(
                "output",
                FlagType::Str,
                "Athena 결과 위치 s3://bucket/prefix/ (비우면 작업 그룹 설정)",
                None,
            );
            if sub == "add" {
                add(
                    "kind",
                    FlagType::Str,
                    "postgresql|redis (URL 과 같아야 함)|opensearch|loki|prometheus|grafana|dynamodb|athena|aws",
                    None,
                );
            } else {
                add("host", FlagType::Str, "호스트", None);
                add("port", FlagType::Str, "포트", None);
                add("db", FlagType::Str, "DB (Redis 는 번호)", None);
            }
        }
        "rm" => add("yes", FlagType::Bool, "묻지 않고 지우기", None),
        "folder" => {
            add("reset", FlagType::Bool, "원래 그룹으로 되돌리기", None);
            add("rename", FlagType::Bool, "폴더 이름 바꾸기 (<옛 폴더> <새 폴더>)", None);
        }
        "test" => {
            add("url", FlagType::Str, "저장하지 않고 시험할 URL", None);
            add("prod", FlagType::Bool, "prod 접속 허용", None);
        }
        _ => return None,
    }
    Some(FlagSet {
        defs,
        check: |_, _| Ok(()),
    })
}

/// 표준 입력이다. tty 면 터미널이라 묻는 글은 바로 stderr 에 쓰고 비밀번호는 가려 읽는다.
pub struct Stdin {
    pub reader: Option<Box<dyn Read + Send>>,
    pub tty: bool,
}

/// `db conn <하위 명령> …` 을 실행하고 종료 코드를 돌려준다.
pub fn run_conn(args: &[String], mut stdin: Stdin, stdout: &mut String, stderr: &mut String) -> i32 {
    let Some(sub) = args.first() else {
        stderr.push_str(CONN_USAGE);
        return EXIT_USAGE;
    };
    if matches!(sub.as_str(), "-h" | "--help" | "help") {
        stdout.push_str(CONN_USAGE);
        return EXIT_OK;
    }
    let Some(fs) = flag_set(sub) else {
        // 이름 자리에 URL(비밀번호)을 잘못 넣었을 수 있어 이름 꼴이 아니면 되풀이하지 않는다
        let shown = if connstore::valid_conn_name(sub) {
            sub.as_str()
        } else {
            "(가림)"
        };
        stderr.push_str(&format!("db conn: 알 수 없는 하위 명령: {shown}\n\n{CONN_USAGE}"));
        return EXIT_USAGE;
    };
    let mut p = Parsed::default();
    let pos = match fs.parse_interleaved(&mut p, &args[1..], stderr, &|out: &mut String| out.push_str(CONN_USAGE)) {
        Ok(pos) => pos,
        Err(ParseError::Help) => return EXIT_OK,
        Err(ParseError::Failed) => return EXIT_USAGE,
    };
    let o = ConnOpts {
        dir: p.strs.get("p").cloned().unwrap_or_else(default_project_dir),
        timeout_ns: p.durs.get("timeout").copied().unwrap_or(30_000_000_000),
        p,
    };
    if o.timeout_ns <= 0 {
        stderr.push_str("db conn: --timeout must be positive\n");
        return EXIT_USAGE;
    }
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            stderr.push_str(&format!("{e}\n"));
            return EXIT_ERR;
        }
    };
    let cx = Cx {
        o: &o,
        rt: &rt,
        timeout: Duration::from_nanos(o.timeout_ns as u64),
    };
    match conn_command(&cx, sub, &pos, &mut stdin, stdout, stderr) {
        Ok(()) => EXIT_OK,
        Err(e) => {
            stderr.push_str(&format!("db conn {sub}: {}\n", printable(e.text(), true)));
            e.code()
        }
    }
}

struct Cx<'a> {
    o: &'a ConnOpts,
    rt: &'a tokio::runtime::Runtime,
    timeout: Duration,
}

impl Cx<'_> {
    /// 접속 시험을 --timeout 안에 돌린다.
    fn test(&self, c: &Conn) -> Result<String, String> {
        self.rt.block_on(async {
            match tokio::time::timeout(self.timeout, test_conn(c)).await {
                Ok(r) => r,
                Err(_) => Err("context deadline exceeded".into()),
            }
        })
    }
}

fn conn_command(cx: &Cx, sub: &str, pos: &[String], stdin: &mut Stdin, stdout: &mut String, stderr: &mut String) -> Result<(), ConnErr> {
    let o = cx.o;
    if sub == "folder" {
        return conn_folder(o, pos, stdout);
    }
    let url = o.s("url");
    if sub == "test" && !url.is_empty() {
        if !pos.is_empty() {
            return Err(usage("<이름> 과 --url 중 하나만 씀"));
        }
        let (mut c, pw) = connstore::parse_conn_url(&url).map_err(usage)?;
        c.password = pw;
        return conn_test(cx, &c, stdout, stderr);
    }
    if pos.len() != 1 {
        return Err(usage("접속 이름 하나가 필요함 (db help conn)"));
    }
    let name = &pos[0];
    // 이름 자리에 URL 을 넣으면 비밀번호가 오류에 찍힐 수 있어 찾거나 되풀이하기 전에 거른다
    if !connstore::valid_conn_name(name) {
        return Err(usage("접속 이름은 영숫자·-·_ 만 받음"));
    }
    let proj = load_project(&o.dir)?;
    let i = proj.conns.iter().position(|c| &c.name == name);
    match (sub, i) {
        ("add", Some(_)) => Err(usage(format!("접속이 이미 있음: {name}"))),
        ("add", None) => conn_save(cx, &proj, None, name, stdin, stdout, stderr),
        (_, None) => Err(usage(format!("접속 없음: {name} (db conns)"))),
        ("test", Some(i)) => conn_test(cx, &proj.conns[i], stdout, stderr),
        (_, Some(i)) if proj.conns[i].source != SOURCE_DB => Err(usage(DATAGRIP_REFUSAL)),
        ("edit", Some(i)) => conn_save(cx, &proj, Some(i), name, stdin, stdout, stderr),
        (_, Some(_)) => {
            // rm
            if !o.b("yes") {
                if !stdin.tty {
                    return Err(usage("--yes 가 필요함 (표준 입력이 터미널이 아님)"));
                }
                let ans = prompt_line(&format!("{name} 접속을 지울까요(.env 비밀번호도) y/N: "));
                if ans.trim() != "y" {
                    return Err(ConnErr::Run("취소함".into()));
                }
            }
            connstore::remove_conn(&proj.dir, name)?;
            stdout.push_str(&format!("지움: {name} (db)\n"));
            Ok(())
        }
    }
}

/// 터미널에 묻고 한 줄을 읽는다. 묻는 글은 결과를 다 모은 뒤가 아니라 바로 보여야 해 stderr 에 곧장 쓴다.
fn prompt_line(msg: &str) -> String {
    let mut e = std::io::stderr();
    let _ = e.write_all(msg.as_bytes());
    let _ = e.flush();
    let mut ans = String::new();
    let _ = std::io::stdin().lock().read_line(&mut ans);
    ans
}

/// add(i 가 None)·edit 를 한다. 플래그로 접속 값을 만들고 시험한 뒤 저장한다.
fn conn_save(
    cx: &Cx,
    proj: &Project,
    i: Option<usize>,
    name: &str,
    stdin: &mut Stdin,
    stdout: &mut String,
    stderr: &mut String,
) -> Result<(), ConnErr> {
    let o = cx.o;
    let url = o.s("url");
    let kind_flag = o.s("kind");
    let mut keep = String::new();
    let mut c = match i {
        Some(i) => {
            let mut c = proj.conns[i].clone();
            keep = std::mem::take(&mut c.password); // 비우면 update_conn 이 .env 값을 그대로 둔다
            let fields = [
                "url",
                "host",
                "port",
                "db",
                "user",
                "group",
                "password-stdin",
                "auth",
                "region",
                "profile",
                "workgroup",
                "output",
            ];
            if !fields.iter().any(|f| o.given(f)) {
                return Err(usage(
                    "고칠 값이 없음 (--url --host --port --db --user --group --auth --region --profile --workgroup --output --password-stdin)",
                ));
            }
            c
        }
        None => {
            let c = Conn {
                kind: Kind::parse(&kind_flag),
                ..Default::default()
            };
            if url.is_empty() && !c.kind.is_some_and(Kind::uses_region) {
                return Err(usage("--url 이 필요함"));
            }
            c
        }
    };
    if let Some(k) = c.kind.filter(|k| k.is_source()) {
        // 저장소는 http(s) URL 이고 사용자 정보는 user·.env 로 나눈다(R60.1)
        if !url.is_empty() {
            let (u, user, pw) = connstore::parse_http_url(&url).map_err(usage)?;
            c.url = u;
            if !user.is_empty() || !pw.is_empty() {
                c.user = user;
                c.password = pw;
            }
        }
        if i.is_none() && !o.given("auth") {
            let opts = connstore::auth_options(k);
            c.auth = opts[0].into();
            if (!c.user.is_empty() || !c.password.is_empty()) && opts.contains(&"basic") {
                c.auth = "basic".into();
            }
        }
        for (f, dst) in [
            ("auth", &mut c.auth),
            ("region", &mut c.region),
            ("profile", &mut c.profile),
            ("workgroup", &mut c.workgroup),
            ("output", &mut c.output),
        ] {
            if o.given(f) {
                *dst = o.s(f);
            }
        }
    } else if !url.is_empty() {
        let (u, pw) = connstore::parse_conn_url(&url).map_err(usage)?;
        c.kind = u.kind;
        c.host = u.host;
        c.port = u.port;
        c.db = u.db;
        c.user = u.user;
        c.password = pw;
        c.sslmode = u.sslmode;
    }
    let flag_kind = Kind::parse(&kind_flag);
    if !kind_flag.is_empty() && flag_kind != c.kind && !flag_kind.is_some_and(Kind::is_source) {
        return Err(usage(format!("--kind {kind_flag} 가 URL 종류({})와 다름", c.kind_str())));
    }
    c.name = name.into();
    for (f, dst) in [
        ("group", &mut c.group),
        ("host", &mut c.host),
        ("db", &mut c.db),
        ("user", &mut c.user),
    ] {
        if o.given(f) {
            *dst = o.s(f);
        }
    }
    if o.given("port") {
        match go_atoi(&o.s("port")) {
            Some(n) if (1..=65535).contains(&n) => c.port = n as u16,
            _ => return Err(usage("--port 는 1~65535 숫자")),
        }
    }
    if o.b("password-stdin") {
        c.password = read_password_stdin(stdin)?;
    }
    connstore::validate_conn(&c).map_err(usage)?;
    if c.env() == "prod" && !o.b("prod") {
        return Err(ConnErr::Refused("prod 접속은 --prod 가 필요함".into()));
    }
    let mut test = c.clone();
    if test.password.is_empty() {
        test.password = keep;
    }
    let w = conn_warning(&c);
    if !w.is_empty() {
        stderr.push_str(&format!("경고: {w}\n"));
    }
    let mut note = String::new();
    match cx.test(&test) {
        Err(e) if !o.b("force") => return Err(ConnErr::Run(format!("접속 시험 실패 (저장하지 않음, --force 로 저장): {e}"))),
        Err(e) => stderr.push_str(&format!("접속 시험 실패 (--force 로 저장): {}\n", printable(&e, true))),
        Ok(v) => note = format!(" · 접속됨 ({v})"),
    }
    let (verb, r) = match i {
        Some(_) => ("고침", connstore::update_conn(&proj.dir, name, c.clone())),
        None => ("추가함", connstore::add_conn(&proj.dir, c.clone())),
    };
    // env_not_updated 면 접속은 저장됐다. 저장을 알리고 손볼 .env 키는 오류로 내 종료 코드 1이다
    let (saved, err) = match r {
        Ok(saved) => (saved, None),
        Err(e) if e.env_not_updated => {
            let mut saved = c;
            saved.source = SOURCE_DB.into();
            (saved, Some(e))
        }
        Err(e) => return Err(e.into()),
    };
    stdout.push_str(&format!("{verb}: {} · {}{note}\n", conn_label(&saved), conn_summary(&saved)));
    match err {
        Some(e) => Err(e.into()),
        None => Ok(()),
    }
}

fn conn_test(cx: &Cx, c: &Conn, stdout: &mut String, stderr: &mut String) -> Result<(), ConnErr> {
    if c.env() == "prod" && !cx.o.b("prod") {
        return Err(ConnErr::Refused("prod 접속은 --prod 가 필요함".into()));
    }
    let w = conn_warning(c);
    if !w.is_empty() {
        stderr.push_str(&format!("경고: {w}\n"));
    }
    let v = cx.test(c).map_err(|e| ConnErr::Run(format!("접속 실패: {e}")))?;
    stdout.push_str(&format!("접속됨 ({v}) · {}\n", conn_summary(c)));
    Ok(())
}

/// 비밀번호 없는 접속 요약이다(PostgreSQL ro@host:5432/app, OpenSearch https://h sigv4 ap-northeast-2).
fn conn_summary(c: &Conn) -> String {
    if c.kind.is_some_and(Kind::is_source) {
        return [kind_label(c.kind), &c.url, &c.auth, &c.region, &c.profile, &c.workgroup, &c.output]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
    }
    let user = if c.user.is_empty() { String::new() } else { format!("{}@", c.user) };
    format!("{} {user}{}:{}/{}", kind_label(c.kind), c.host, c.port, c.db)
}

/// --password-stdin 의 비밀번호다. 터미널이면 가려 읽고, 아니면 끝 줄바꿈 하나를 뗀 표준 입력 전체다.
fn read_password_stdin(stdin: &mut Stdin) -> Result<String, ConnErr> {
    let b = if stdin.tty {
        read_hidden("비밀번호: ").map_err(|e| ConnErr::Run(e.to_string()))?.into_bytes()
    } else {
        let Some(r) = stdin.reader.as_mut() else {
            return Err(usage("--password-stdin 에 줄 표준 입력이 없음"));
        };
        let mut b = Vec::new();
        r.take(CONN_MAX_PASSWORD as u64 + 1)
            .read_to_end(&mut b)
            .map_err(|e| ConnErr::Run(e.to_string()))?;
        b
    };
    if b.len() > CONN_MAX_PASSWORD {
        return Err(usage("비밀번호가 64KiB 를 넘음"));
    }
    let s = String::from_utf8_lossy(&b);
    let pw = s.strip_suffix('\n').unwrap_or(&s);
    let pw = pw.strip_suffix('\r').unwrap_or(pw);
    if pw.is_empty() {
        return Err(usage("비밀번호가 비었음"));
    }
    Ok(pw.to_string())
}

/// 터미널에서 메아리 없이 한 줄을 읽는다(Go term.ReadPassword). 끝 줄바꿈은 뗀다.
fn read_hidden(msg: &str) -> std::io::Result<String> {
    let mut e = std::io::stderr();
    e.write_all(msg.as_bytes())?;
    e.flush()?;
    // SAFETY: 표준 입력(fd 0)의 termios 를 읽고 ECHO 만 끈 뒤 되돌린다. 구조체는 tcgetattr 가 채운다
    let mut old: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(0, &mut old) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut quiet = old;
    quiet.c_lflag &= !libc::ECHO;
    quiet.c_lflag |= libc::ECHONL;
    unsafe { libc::tcsetattr(0, libc::TCSANOW, &quiet) };
    let mut line = String::new();
    let r = std::io::stdin().lock().read_line(&mut line);
    unsafe { libc::tcsetattr(0, libc::TCSANOW, &old) };
    let _ = e.write_all(b"\n");
    r?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

/// `db conn folder` 다(R66.4). 접속에 붙지 않아 --prod 가 필요 없다.
fn conn_folder(o: &ConnOpts, pos: &[String], stdout: &mut String) -> Result<(), ConnErr> {
    let proj = load_project(&o.dir)?;
    if o.b("rename") {
        if o.b("reset") || pos.len() != 2 || pos[1].trim().is_empty() {
            return Err(usage("--rename <옛 폴더> <새 폴더>"));
        }
        let conns: Vec<&Conn> = proj.conns.iter().filter(|c| c.kind.is_some() && folder_of(c) == pos[0]).collect();
        if conns.is_empty() {
            return Err(usage(format!("폴더 없음: {} (db conns)", printable(&pos[0], true))));
        }
        connstore::set_folder(&proj.dir, &conns, &pos[1])?;
        stdout.push_str(&format!(
            "폴더 이름 바꿈: {} → {} (접속 {})\n",
            printable(&pos[0], false),
            pos[1].trim(),
            conns.len()
        ));
        return Ok(());
    }
    let reset = o.b("reset");
    // DataGrip 접속 이름은 공백·. 이 있을 수 있어 이름 규칙을 보지 않는다. 오류·출력에는 printable 로만 되풀이한다
    if pos.len() != if reset { 1 } else { 2 } {
        return Err(usage("db conn folder <이름> <폴더> | <이름> --reset"));
    }
    let Some(c) = proj.conn_by_name(&pos[0]) else {
        return Err(usage(format!("접속 없음: {} (db conns)", printable(&pos[0], false))));
    };
    let folder = if reset { "" } else { pos[1].as_str() };
    connstore::set_folder(&proj.dir, &[c], folder)?;
    if reset || folder.trim().is_empty() {
        stdout.push_str(&format!(
            "되돌림: {} → {}\n",
            printable(&c.name, false),
            printable(group_of(c), false)
        ));
    } else {
        stdout.push_str(&format!("옮김: {} → {}\n", printable(&c.name, false), folder.trim()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(kind: Kind, url: &str, auth: &str) -> Conn {
        Conn {
            kind: Some(kind),
            url: url.into(),
            auth: auth.into(),
            ..Default::default()
        }
    }

    #[test]
    fn 요약에는_비밀번호가_없다() {
        let pg = Conn {
            kind: Some(Kind::Pg),
            host: "h".into(),
            port: 5432,
            db: "app".into(),
            user: "ro".into(),
            password: "secret".into(),
            ..Default::default()
        };
        assert_eq!(conn_summary(&pg), "PostgreSQL ro@h:5432/app");
        let mut os = conn(Kind::OpenSearch, "https://h", "sigv4");
        os.region = "ap-northeast-2".into();
        assert_eq!(conn_summary(&os), "OpenSearch https://h sigv4 ap-northeast-2");
    }
}
