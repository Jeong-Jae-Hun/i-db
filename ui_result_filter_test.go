package main

import (
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

// ─── R14.3·R14.4·R16.4 결과 정렬·거르기·WHERE·ORDER BY 줄 ───

// headerX 는 결과 표 머리에서 열 col 이 있는 grid 기준 x 다.
func headerX(t *testing.T, tab *queryTab, w int, col string) int {
	t.Helper()
	for x := range w {
		if c, ok := tab.grid.HeaderAt(x, 0); ok && c == col {
			return x
		}
	}
	t.Fatalf("%s 머리를 찾지 못함", col)
	return -1
}

// resultAt 은 결과 영역 위 줄(resultLines) 아래 grid 기준 (x, y) 를 화면 좌표 클릭으로 바꾼다.
func resultAt(m *model, tab *queryTab, x, y int) tea.MouseMsg {
	l := m.layout()
	return press(l.treeW+2+1+x, l.edH+5+len(m.resultLines(tab, l.rightW))+y)
}

// reload 는 테이블 보기 다시 읽기가 끝난 것처럼 같은 페이지 결과를 넘긴다.
func reload(m *model, tab *queryTab) {
	res := &db.Result{Cols: []string{"id", "status"}, Types: []string{"int4", "text"}, Rows: [][]string{{"1", "ACTIVE"}, {"2", "NULL"}}}
	m.Update(resultMsg{tab: tab.id, seq: tab.seq, conn: 1, kind: rkTable, res: res, rev: tab.grid.Revision()})
}

func TestHeaderClick_일반_결과는_받은_행을_정렬하고_돈다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	show(m, &db.Result{Cols: []string{"id", "name"}, Types: []string{"int4", "text"}, Rows: [][]string{{"1", "b"}, {"2", "NULL"}, {"3", "a"}}})
	tab := m.cur()
	x := headerX(t, tab, m.layout().rightW, "name")
	for _, want := range []string{"정렬 name ↑", "정렬 name ↓", ""} {
		if _, cmd := m.Update(resultAt(m, tab, x, 0)); cmd != nil || tab.running != nil {
			t.Fatal("일반 결과의 머리 클릭은 다시 실행하지 않는다")
		}
		lines := m.resultLines(tab, m.layout().rightW)
		got := ""
		if len(lines) > 0 {
			got = ansi.Strip(lines[0])
		}
		if got != want {
			t.Fatalf("표 위 줄: got %q want %q", got, want)
		}
	}
	m.Update(resultAt(m, tab, x, 0))
	m.Update(key("s")) // 같은 열 s 는 다음 단계(내림)
	if row := tab.grid.CurrentRow(); tab.grid.Summary() != "정렬 name ↓" || row == nil {
		t.Fatalf("s 와 머리 클릭은 같은 순환: %q", tab.grid.Summary())
	}
}

func TestTableView_s_는_커서_열의_서버_ORDER_BY_로_다시_읽는다(t *testing.T) {
	m := testModel(t)
	tab := tableResult(m)
	m.setFocus(fResult)
	m.Update(key("l"))
	if _, cmd := m.Update(key("s")); cmd == nil || tab.running == nil || !strings.Contains(tab.tv.SQL(), `ORDER BY "status" ASC`) {
		t.Fatalf("s 는 서버 정렬: %q", tab.tv.SQL())
	}
	reload(m, tab)
	if tab.grid.CurrentCol() != "status" || tab.grid.Summary() != "" {
		t.Fatalf("다시 읽어도 커서 열을 지키고 받은 행은 정렬하지 않는다: %q %q", tab.grid.CurrentCol(), tab.grid.Summary())
	}
	m.Update(key("s"))
	if !strings.Contains(tab.tv.SQL(), `ORDER BY "status" DESC`) || !strings.Contains(tab.tv.View(200), "ORDER BY status DESC") {
		t.Fatalf("두 번째 s 는 내림차순, 줄에도 보인다: %q / %q", tab.tv.SQL(), tab.tv.View(200))
	}
}

func TestTableView_슬래시_짧은_꼴은_WHERE_로_다시_읽고_esc_는_지운다(t *testing.T) {
	m := testModel(t)
	tab := tableResult(m)
	reload(m, tab) // 열 이름·타입을 안다
	m.setFocus(fResult)
	m.Update(key("/"))
	if !tab.tv.Typing() {
		t.Fatal("/ 는 WHERE 입력을 연다")
	}
	m.Update(key("status=ACTIVE"))
	if _, cmd := m.Update(key("enter")); cmd == nil || !strings.Contains(tab.tv.SQL(), `WHERE "status"::text = 'ACTIVE'`) {
		t.Fatalf("짧은 꼴은 SQL 조건으로: %q", tab.tv.SQL())
	}
	reload(m, tab)
	if _, cmd := m.Update(tea.KeyMsg{Type: tea.KeyEsc}); cmd == nil || strings.Contains(tab.tv.SQL(), "WHERE") {
		t.Fatalf("esc 는 건 조건을 지우고 다시 읽는다: %q", tab.tv.SQL())
	}
}

func TestTableView_WHERE_ORDER_BY_칸을_누르면_입력을_열고_적용한다(t *testing.T) {
	m := testModel(t)
	tab := tableResult(m)
	reload(m, tab)
	m.Update(resultAt(m, tab, 2, -1))
	if !tab.tv.Typing() || !strings.HasPrefix(tab.tv.View(80), "WHERE █") {
		t.Fatalf("WHERE 칸 클릭: %q", tab.tv.View(80))
	}
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	if tab.tv.Typing() || tab.running != nil {
		t.Fatal("esc 는 입력만 닫는다")
	}
	m.Update(resultAt(m, tab, len("WHERE (없음)")+3+2, -1))
	if !strings.HasPrefix(tab.tv.View(80), "ORDER BY █") {
		t.Fatalf("ORDER BY 칸 클릭: %q", tab.tv.View(80))
	}
	m.Update(key("st"))
	m.Update(tea.KeyMsg{Type: tea.KeyTab})
	m.Update(key(" desc"))
	if _, cmd := m.Update(key("enter")); cmd == nil || !strings.Contains(tab.tv.SQL(), `ORDER BY "status" DESC`) {
		t.Fatalf("Tab 으로 채운 ORDER BY 적용: %q", tab.tv.SQL())
	}
}
