package main

import (
	"bytes"
	"context"
	"encoding/json"
	"slices"
	"strings"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/editor"
	"github.com/Jeong-Jae-Hun/i-db/internal/grid"
	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

// ─── 결과 표 붙여넣기·칸 입력 도우미 (R48·R49) ───
// 붙여넣기·enum 고르기·긴 글 편집은 표의 보류 중인 바꿈에 값을 둘 뿐이라 반영은 R15 제출을 그대로 탄다.

// enumMsg 는 접속 conn 의 사용자 타입(OID 숫자)이 enum 인지 읽은 결과다. enum 이 아니면 빈 목록이다.
type enumMsg struct {
	conn   int
	db     string
	labels map[string][]string
	err    error
}

// longEdit 는 칸 편집 중 Ctrl+O 로 연 전체 화면 편집기다(R49.3). tab 은 그 칸이 있는 탭 id 다.
type longEdit struct {
	ed   editor.Model
	tab  int
	json bool
	orig string
}

// enumSet 은 접속 conn 에서 enum 으로 읽은 타입 이름이다. grid.SetEnumTypes 에 넘긴다.
func (m *model) enumSet(conn int) map[string]bool {
	set := map[string]bool{}
	for typ, labels := range m.enums[conn] {
		if len(labels) > 0 {
			set[typ] = true
		}
	}
	return set
}

// loadEnums 는 탭 결과의 사용자 타입 중 아직 모르는 것이 enum 인지 읽는 명령이다(접속·타입마다 한 번).
// 이미 아는 타입은 바로 표에 알린다.
func (m *model) loadEnums(t *queryTab, conn int, res *db.Result) tea.Cmd {
	if conn < 0 || m.conn(conn).Kind != db.KindPG {
		return nil
	}
	t.grid.SetEnumTypes(m.enumSet(conn))
	var want []string
	if m.enums[conn] == nil {
		m.enums[conn] = map[string][]string{}
	}
	var types []string
	if res != nil {
		types = res.Types
	}
	// OID 숫자로 실린 타입만 사용자 타입이다(내장 타입은 이름으로 실린다)
	for _, typ := range types {
		if _, ok := m.enums[conn][typ]; !ok && typ != "" && strings.Trim(typ, "0123456789") == "" {
			m.enums[conn][typ] = nil // 읽는 중이다. 다시 묻지 않는다
			want = append(want, typ)
		}
	}
	if len(want) == 0 {
		return nil
	}
	dbName := m.conn(conn).DB
	return withPG(m.life, m.clients, m.conn(conn), func(ctx context.Context, pg *db.PGConn, err error) tea.Msg {
		if err != nil {
			return enumMsg{conn: conn, db: dbName, err: err}
		}
		out := map[string][]string{}
		for _, typ := range want {
			labels, err := pg.EnumLabels(ctx, typ)
			if err != nil {
				return enumMsg{conn: conn, db: dbName, err: err}
			}
			out[typ] = labels
		}
		return enumMsg{conn: conn, db: dbName, labels: out}
	})
}

// pickEnum 은 enum 칸의 값을 고르는 창을 연다(R49.2). 고르면 SetCell 로 보류 중인 바꿈에 둔다.
func (m *model) pickEnum(t *queryTab, a grid.PickEnum) {
	labels := m.enums[t.viewConn][a.Type]
	if len(labels) == 0 {
		m.status = "enum 값을 아직 읽지 못함"
		return
	}
	items := append(slices.Clone(labels), "NULL")
	m.openPicker("enum 값 ", items, nil, func(v string) tea.Cmd {
		var val *string
		if v != "NULL" {
			val = &v
		}
		if err := t.grid.SetCell(val); err != nil {
			m.status = err.Error()
			return nil
		}
		m.status = "enum 값 둠: " + v
		return nil
	}, nil)
	m.pickerCursor = max(0, slices.Index(items, a.Current))
}

// openLong 은 칸 편집 중 Ctrl+O 로 전체 화면 편집기를 연다(R49.3).
func (m *model) openLong(t *queryTab, a grid.EditLong) {
	ed := editor.New()
	ed.SetValue(a.Value)
	ed.SetSize(max(10, m.w-2), max(3, m.h-3))
	ed.Focus()
	m.long = &longEdit{ed: ed, tab: t.id, json: a.JSON, orig: a.Value}
	m.overlay = oLong
	m.status = m.label("Ctrl+S 저장 · esc 버림")
	if a.JSON {
		m.status += m.label(" · F8 JSON 정렬")
	}
}

// longKey 는 전체 화면 편집기의 키다. Ctrl+S 는 (JSON 이면 검사한 뒤) 칸에 두고, esc 는 버린다.
func (m *model) longKey(k tea.KeyMsg) tea.Cmd {
	l := m.long
	switch k.String() {
	case "esc":
		m.overlay, m.long = oNone, nil
		m.status = "편집 버림"
		return nil
	case "f8":
		if !l.json {
			m.status = "JSON 열에서만 정렬"
			return nil
		}
		var buf bytes.Buffer
		if err := json.Indent(&buf, []byte(l.ed.Value()), "", "  "); err != nil {
			m.status = "JSON 이 아님: " + err.Error()
			return nil
		}
		l.ed.SetValue(buf.String())
		m.status = "JSON 정렬함"
		return nil
	case "ctrl+s":
		v := l.ed.Value()
		if l.json && !json.Valid([]byte(v)) {
			var x any
			err := json.Unmarshal([]byte(v), &x)
			m.status = "JSON 이 아님: " + textutil.Printable(err.Error(), false)
			return nil
		}
		m.overlay, m.long = oNone, nil
		t := m.findTab(l.tab)
		switch {
		case t == nil:
			m.status = "탭이 닫혀 두지 않음"
		case v == l.orig:
			m.status = "바뀐 것이 없어 두지 않음"
		default:
			if err := t.grid.SetCell(&v); err != nil {
				m.status = err.Error()
				return nil
			}
			m.status = m.label("칸에 둠 (Ctrl+S 로 미리 보기)")
		}
		return nil
	}
	var cmd tea.Cmd
	l.ed, cmd = l.ed.Update(k)
	return cmd
}

// longView 는 전체 화면 편집기 화면이다.
func (m *model) longView() string {
	l := m.long
	l.ed.SetSize(max(10, m.w-2), max(3, m.h-3))
	title := "긴 값 편집"
	if l.json {
		title += " (JSON)"
	}
	head := appBadge.Render(" db ") + barStyle.Render(" "+title)
	return m.bar(head, "") + "\n" + lipgloss.NewStyle().Padding(0, 1).Render(l.ed.View()) + "\n" +
		m.bar(barStyle.Render(" "+textutil.Printable(m.status, false)), "")
}
