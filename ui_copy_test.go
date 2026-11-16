package main

import (
	"bytes"
	"encoding/base64"
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/clipboard"
	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/editor"
	"github.com/Jeong-Jae-Hun/i-db/internal/grid"
)

// ─── R71 복사 꼴 고르기 ───

func copyResult() *db.Result {
	return &db.Result{Cols: []string{"id", "name"}, Types: []string{"int4", "text"}, Rows: [][]string{{"1", "a"}, {"2", "b"}, {"3", "c"}}}
}

// pickLabel 은 고르기 창에서 이름이 label 인 항목으로 커서를 옮기고 Enter 를 누른다.
func pickLabel(t *testing.T, m *model, label string) tea.Cmd {
	t.Helper()
	i := strings.Index(strings.Join(m.pickerItems, "\n")+"\n", label+"\n")
	if i < 0 {
		t.Fatalf("항목 없음 %q: %v", label, m.pickerItems)
	}
	m.pickerCursor = strings.Count(strings.Join(m.pickerItems, "\n")[:i], "\n")
	_, cmd := m.Update(key("enter"))
	return cmd
}

func decodeOSC(t *testing.T, raw string) string {
	t.Helper()
	start := strings.LastIndex(raw, ";") + 1
	dec, err := base64.StdEncoding.DecodeString(raw[start:strings.LastIndex(raw, "\a")])
	must(t, err)
	return string(dec)
}

func TestCopyMenu_CtrlK는_고르기_창을_열고_고른_꼴로_복사하며_마지막_꼴을_기억한다(t *testing.T) {
	var buf bytes.Buffer
	old := clipboardOut
	clipboardOut = &buf
	t.Cleanup(func() { clipboardOut = old })
	m := testModel(t)
	show(m, copyResult())
	m.setFocus(fResult)

	m.Update(tea.KeyMsg{Type: tea.KeyCtrlK})
	if m.overlay != oPicker || m.pickerItems[m.pickerCursor] != "현재 행 · TSV (헤더)" {
		t.Fatalf("처음은 현재 행 TSV: overlay=%v %v", m.overlay, m.pickerItems)
	}
	must(t, nilMsg(pickLabel(t, m, "보이는 행 전체 · CSV (헤더)")))
	if got := decodeOSC(t, buf.String()); got != "id,name\n1,a\n2,b\n3,c\n" || m.status != "복사함 (4줄) · 보이는 행 전체 · CSV (헤더)" {
		t.Fatalf("복사: %q status=%q", got, m.status)
	}

	m.Update(tea.KeyMsg{Type: tea.KeyCtrlK})
	if got := m.pickerItems[m.pickerCursor]; got != "현재 행 · CSV (헤더)" {
		t.Fatalf("마지막 꼴(CSV)을 현재 행에서 고른다: %q", got)
	}
	m.Update(key("esc"))
	m.Update(key("V"))
	m.Update(key("j"))
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlK})
	if got := m.pickerItems[m.pickerCursor]; got != "선택 · CSV (헤더)" {
		t.Fatalf("선택 중이면 선택이 기본: %q", got)
	}
	m.Update(key("esc"))
}

func TestCopyMenu_대상에_없는_꼴은_그_꼴이_있는_대상을_고른다(t *testing.T) {
	m := testModel(t)
	show(m, copyResult())
	m.setFocus(fResult)
	m.copyFormat = grid.FormatIN // 열 대상에만 있다
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlK})
	if got := m.pickerItems[m.pickerCursor]; got != "현재 열 · IN 목록" {
		t.Fatalf("IN 목록: %q", got)
	}
}

func TestCopyMenu_오른쪽_클릭은_그_칸으로_옮기고_현재_행_대상으로_창을_연다(t *testing.T) {
	m := testModel(t)
	show(m, copyResult())
	m.setFocus(fEditor)
	m.copyFormat = grid.FormatMarkdown
	l := m.layout()
	rightX := l.treeW + 2
	top := l.edH + 5 + len(m.resultLines(m.cur(), l.rightW))
	// 결과 본문의 셋째 행(id=3) 첫 칸을 찾는다. 표 왼쪽에는 행 번호 자리가 있다
	gx := 0
	for !m.cur().grid.CellAt(gx, 3) && gx < l.rightW {
		gx++
	}
	x, y, rows := rightX+1+gx, -1, 0
	for sy := top; sy <= l.bodyH; sy++ {
		if m.cur().grid.CellAt(gx, sy-top) {
			if rows == 2 {
				y = sy
				break
			}
			rows++
		}
	}
	if y < 0 {
		t.Fatal("결과 칸을 찾지 못함")
	}
	m.Update(tea.MouseMsg{X: x, Y: y, Action: tea.MouseActionPress, Button: tea.MouseButtonRight})
	if m.overlay != oPicker || m.focus != fResult || m.pickerItems[m.pickerCursor] != "현재 행 · Markdown 표" {
		t.Fatalf("overlay=%v focus=%v cursor=%q", m.overlay, m.focus, m.pickerItems[m.pickerCursor])
	}
	// 오른쪽 클릭 창 끝에는 연결된 곳 찾기(R73)가 붙는다
	if m.pickerItems[len(m.pickerItems)-1] != linkedMenuItem {
		t.Fatalf("끝 항목: %v", m.pickerItems)
	}
	cmd := pickLabel(t, m, "현재 행 · TSV (헤더)")
	if cmd == nil || m.status != "복사함 (2줄) · 현재 행 · TSV (헤더)" {
		t.Fatalf("status=%q", m.status)
	}
	if got := gridCell(t, m); got != "3" {
		t.Fatalf("커서가 누른 칸으로: %q", got)
	}
	// 머리 줄 오른쪽 클릭은 창을 열지 않는다
	m.Update(tea.MouseMsg{X: x, Y: top, Action: tea.MouseActionPress, Button: tea.MouseButtonRight})
	if m.overlay != oNone {
		t.Fatal("머리 줄에서 창을 열었다")
	}
}

func TestCopy_너무_큰_글은_내보내기를_권한다(t *testing.T) {
	m := testModel(t)
	m.Update(editor.ClipboardErrMsg{Err: clipboard.TooBigError{Bytes: 12 << 20}})
	if m.status != "너무 커서 복사하지 않음 (12.0 MB) — e/E 로 내보내기" {
		t.Fatalf("status=%q", m.status)
	}
	big := strings.Repeat("x", clipboard.MaxBytes+1)
	msg := copyCmd(big)()
	if e, ok := msg.(editor.ClipboardErrMsg); !ok || e.Err.(clipboard.TooBigError).Bytes != len(big) {
		t.Fatalf("msg=%v", msg)
	}
}
