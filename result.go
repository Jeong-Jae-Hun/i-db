package main

import (
	"strings"
	"time"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

// ─── 실행 결과 ───

// refresh 는 제출·백엔드 신호 뒤 탭의 표를 다시 읽는다. 테이블 보기면 지금 페이지, 아니면 마지막 읽기 실행이다.
func (m *model) refresh(t *queryTab) tea.Cmd {
	switch {
	case t.tv != nil:
		return m.guard(m.tablePage(t))
	case t.last != nil:
		p := *t.last
		return m.guard(p)
	}
	return nil
}

// shown 은 지금 결과 표에 보이는 결과다(고정·문장별 결과를 고르면 그 결과).
func (t *queryTab) shown() *db.Result {
	if t.view >= 0 && t.view < len(t.subs) {
		return t.subs[t.view].res
	}
	return t.res
}

// resultLines 는 결과 표 위에 붙는 줄(결과 작은 탭·테이블 보기 줄)이다. 그리기와 마우스 좌표가 같이 쓴다.
func (m *model) resultLines(t *queryTab, w int) []string {
	var lines []string
	if len(t.subs) > 0 {
		parts := []string{" 결과 "}
		for _, s := range t.subs {
			mark := ""
			if s.pinned {
				mark = "● "
			}
			parts = append(parts, " "+mark+textutil.Printable(s.title, false)+" ")
		}
		for i := range parts {
			st := tabInactive
			if i-1 == t.view {
				st = tabActive
			}
			parts[i] = st.Render(parts[i])
		}
		lines = append(lines, truncate(strings.Join(parts, " "), w))
	}
	if t.tv != nil {
		lines = append(lines, t.tv.View(w))
	}
	return lines
}

// tick 은 열린 트랜잭션이 있으면 1초 뒤 다시 그리게 한다. 한 번에 하나만 돈다.
func (m *model) tick() tea.Cmd {
	open := false
	for _, v := range m.inTx {
		open = open || v
	}
	if !open || m.ticking {
		return nil
	}
	m.ticking = true
	return tea.Tick(txTick, func(time.Time) tea.Msg { return tickMsg{} })
}
