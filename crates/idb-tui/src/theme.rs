//! 색. 256색 번호로 둔다. truecolor 가 없는 터미널(tmux 기본값 등)에서도 같은 색이 나온다(Go theme.go 와 같은 번호).

use ratatui::style::{Color, Modifier, Style};

pub const PROD: Color = Color::Indexed(203);
pub const STG: Color = Color::Indexed(221);
pub const LOCAL: Color = Color::Indexed(245);
pub const DIM: Color = Color::Indexed(242);
pub const BORDER: Color = Color::Indexed(238);
pub const ACCENT: Color = Color::Indexed(75);
pub const TEXT: Color = Color::Indexed(252);
pub const BAR: Color = Color::Indexed(235);
pub const WHITE: Color = Color::Indexed(231);
pub const BLACK: Color = Color::Indexed(16);
pub const PG: Color = Color::Indexed(75);
pub const REDIS: Color = Color::Indexed(174);
pub const SOURCE: Color = Color::Indexed(150);
pub const GREEN: Color = Color::Indexed(114);

pub fn bar() -> Style {
    Style::new().bg(BAR).fg(TEXT)
}

pub fn hint() -> Style {
    Style::new().bg(BAR).fg(DIM)
}

pub fn dim() -> Style {
    Style::new().fg(DIM)
}

pub fn err() -> Style {
    Style::new().fg(PROD)
}

pub fn app_badge() -> Style {
    Style::new().bg(ACCENT).fg(BLACK).add_modifier(Modifier::BOLD)
}

pub fn tx_badge() -> Style {
    Style::new().bg(BAR).fg(STG).add_modifier(Modifier::BOLD)
}

pub fn tab_active() -> Style {
    app_badge()
}

pub fn tab_inactive() -> Style {
    Style::new().bg(BAR).fg(DIM)
}

pub fn confirm_bar() -> Style {
    Style::new().bg(PROD).fg(WHITE).add_modifier(Modifier::BOLD)
}

pub fn help_key() -> Style {
    Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub fn help_title() -> Style {
    Style::new().fg(TEXT).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
}

/// 헤더의 환경 표시다. prod 는 쓰기 전에 눈에 띄어야 해서 배경을 칠한다.
pub fn env_badge(env: &str) -> Style {
    let s = Style::new().add_modifier(Modifier::BOLD);
    match env {
        "prod" => s.bg(PROD).fg(WHITE),
        "stg" => s.bg(STG).fg(BLACK),
        _ => s.bg(LOCAL).fg(BLACK),
    }
}

/// 트리·팔레트의 접속 글자색이다(R2.2).
pub fn env_style(env: &str) -> Style {
    match env {
        "prod" => Style::new().fg(PROD),
        "stg" => Style::new().fg(STG),
        _ => Style::new(),
    }
}

pub fn kind_color(kind: &str) -> Color {
    match kind {
        "postgresql" | "pg" => PG,
        "redis" => REDIS,
        _ => SOURCE,
    }
}

/// 트리 아이콘은 Nerd Font 없이도 나오는 기하 도형만 쓴다. 모두 2칸이라 마우스 위치 계산이 깊이로만 정해진다.
pub fn kind_icon(kind: &str) -> &'static str {
    match kind {
        "postgresql" | "pg" => "◆ ",
        "redis" => "◈ ",
        _ => "◇ ",
    }
}

pub fn kind_label(kind: &str) -> &'static str {
    match kind {
        "postgresql" | "pg" => "PostgreSQL",
        "redis" => "Redis",
        "opensearch" => "OpenSearch",
        "loki" => "Loki",
        "prometheus" => "Prometheus",
        "grafana" => "Grafana",
        "dynamodb" => "DynamoDB",
        "athena" => "Athena",
        "aws" => "AWS",
        _ => "",
    }
}

/// 대시보드·흐름·보드의 경고 칸(Go dashWarnStyle)
pub fn warn() -> Style {
    Style::new().fg(STG).add_modifier(Modifier::BOLD)
}

/// 대시보드 연결됨(Go dashOKStyle)
pub fn ok() -> Style {
    Style::new().fg(GREEN)
}
