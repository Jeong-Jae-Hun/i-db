package main

import (
	"strings"

	"github.com/charmbracelet/lipgloss"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

// 색은 256색 번호로 둔다. truecolor 가 없는 터미널(tmux 기본값 등)에서도 같은 색이 나온다.
var (
	colProd   = lipgloss.Color("203")
	colStg    = lipgloss.Color("221")
	colLocal  = lipgloss.Color("245")
	colDim    = lipgloss.Color("242")
	colBorder = lipgloss.Color("238")
	colAccent = lipgloss.Color("75")
	colText   = lipgloss.Color("252")
	colBar    = lipgloss.Color("235")
	colWhite  = lipgloss.Color("231")
	colBlack  = lipgloss.Color("16")
	colPG     = lipgloss.Color("75")
	colRedis  = lipgloss.Color("174")
	colSource = lipgloss.Color("150") // 읽기 전용 저장소(R60)
)

var (
	barStyle    = lipgloss.NewStyle().Background(colBar).Foreground(colText)
	hintStyle   = lipgloss.NewStyle().Background(colBar).Foreground(colDim)
	dimStyle    = lipgloss.NewStyle().Foreground(colDim)
	appBadge    = lipgloss.NewStyle().Background(colAccent).Foreground(colBlack).Bold(true)
	txBadge     = lipgloss.NewStyle().Background(colBar).Foreground(colStg).Bold(true)
	tabActive   = lipgloss.NewStyle().Background(colAccent).Foreground(colBlack).Bold(true)
	tabInactive = lipgloss.NewStyle().Background(colBar).Foreground(colDim)
	errStyle    = lipgloss.NewStyle().Foreground(colProd)
	confirmBar  = lipgloss.NewStyle().Background(colProd).Foreground(colWhite).Bold(true)
	helpKey     = lipgloss.NewStyle().Foreground(colAccent).Bold(true)
	helpTitle   = lipgloss.NewStyle().Foreground(colText).Bold(true).Underline(true)
	helpBox     = lipgloss.NewStyle().Border(lipgloss.RoundedBorder()).BorderForeground(colAccent).Padding(0, 2)
)

// envBadges 는 헤더의 환경 표시다. prod 는 쓰기 전에 눈에 띄어야 해서 배경을 칠한다.
var envBadges = map[string]lipgloss.Style{
	"prod":  lipgloss.NewStyle().Background(colProd).Foreground(colWhite).Bold(true),
	"stg":   lipgloss.NewStyle().Background(colStg).Foreground(colBlack).Bold(true),
	"local": lipgloss.NewStyle().Background(colLocal).Foreground(colBlack).Bold(true),
}

// 트리 아이콘은 Nerd Font 없이도 나오는 기하 도형만 쓴다. 모두 2칸(글자+공백)이라 마우스 위치 계산이 깊이로만 정해진다.
var (
	kindIcons = map[nodeKind]string{nSchema: "▣ ", nTable: "▦ ", nKey: "◇ ", nFolder: "▤ ", nQueries: "▥ ", nFile: "▢ ", nSource: "▫ "}
	connIcons = map[db.Kind]string{db.KindPG: "◆ ", db.KindRedis: "◈ ", db.KindOpenSearch: "◇ ", db.KindLoki: "◇ ",
		db.KindPrometheus: "◇ ", db.KindGrafana: "◇ ", db.KindDynamoDB: "◇ ", db.KindAthena: "◇ ", db.KindAWS: "◇ "}
	connColor = map[db.Kind]lipgloss.Color{db.KindPG: colPG, db.KindRedis: colRedis, db.KindOpenSearch: colSource, db.KindLoki: colSource,
		db.KindPrometheus: colSource, db.KindGrafana: colSource, db.KindDynamoDB: colSource, db.KindAthena: colSource, db.KindAWS: colSource}
	connKinds = map[db.Kind]string{db.KindPG: "PostgreSQL", db.KindRedis: "Redis", db.KindOpenSearch: "OpenSearch", db.KindLoki: "Loki",
		db.KindPrometheus: "Prometheus", db.KindGrafana: "Grafana", db.KindDynamoDB: "DynamoDB", db.KindAthena: "Athena", db.KindAWS: "AWS"}
)

type paneSpec struct {
	title, info, body string
	w, h              int // 테두리 안쪽 크기
	focused           bool
	hot               bool // 이 영역의 경계를 마우스로 끄는 중(R46.1)
}

func envStyle(c db.Conn) lipgloss.Style {
	switch c.Env() {
	case "prod":
		return lipgloss.NewStyle().Foreground(colProd)
	case "stg":
		return lipgloss.NewStyle().Foreground(colStg)
	}
	return lipgloss.NewStyle()
}

// pane 은 위 테두리에 제목(왼쪽)과 정보(오른쪽)를 넣은 둥근 테두리 영역을 그린다.
// lipgloss 테두리는 제목을 지원하지 않아 위 줄만 직접 만들고 나머지 세 변은 lipgloss 에 맡긴다.
func pane(p paneSpec) string {
	border, titleSt := colBorder, lipgloss.NewStyle().Foreground(colDim)
	if p.focused {
		border, titleSt = colAccent, lipgloss.NewStyle().Foreground(colAccent).Bold(true)
	}
	b := lipgloss.RoundedBorder()
	// 색이 없는 터미널에서도 보이게 굵은 선으로도 바꾼다
	if p.hot {
		border, b = colStg, lipgloss.ThickBorder()
	}
	bs := lipgloss.NewStyle().Foreground(border)
	title := truncate(" "+p.title+" ", max(0, p.w-1))
	info := ""
	if room := p.w - 2 - lipgloss.Width(title); p.info != "" && room >= 4 {
		info = truncate(" "+p.info+" ", room)
	}
	fill := max(0, p.w-1-lipgloss.Width(title)-lipgloss.Width(info))
	top := bs.Render(b.TopLeft+b.Top) + titleSt.Render(title) + bs.Render(strings.Repeat(b.Top, fill)) +
		dimStyle.Render(info) + bs.Render(b.TopRight)
	body := lipgloss.NewStyle().Border(b, false, true, true, true).BorderForeground(border).
		Width(p.w).Height(p.h).MaxHeight(p.h + 1).Render(p.body)
	return top + "\n" + body
}
