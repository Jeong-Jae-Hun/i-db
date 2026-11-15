package main

import (
	"strings"
	"time"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/params"
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

// resultLines 는 결과 표 위에 붙는 줄(결과 작은 탭, 테이블 보기의 WHERE·ORDER BY 줄 또는 일반 결과의 정렬·거르기 줄)이다. 그리기와 마우스 좌표가 같이 쓴다.
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
	switch sum := t.grid.Summary(); {
	case t.tv != nil:
		lines = append(lines, t.tv.View(w))
	case sum != "":
		lines = append(lines, dimStyle.Render(truncate(sum, w)))
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

// ─── 정렬·거르기 서버로 다시 읽기 (R14.5) ───

// requery 는 일반 결과의 정렬·거르기가 지금 결과를 만든 것과 달라졌으면 원래 문장을 감싸 서버에서 다시 읽는다.
// 거르기 글을 치는 동안이나 다시 읽을 수 없는 결과(R33.1 F7 과 같은 기준)면 nil 이고 받은 행 안의 결과가 남는다.
func (m *model) requery(t *queryTab) tea.Cmd {
	q := t.grid.Query()
	conn := t.exportConn
	// 다른 실행이 돌고 있으면 받은 행 안에서만 한다. 돌고 있는 것이 다시 읽기면 start 가 그것을 취소하고 새로 보낸다.
	// 탭 접속을 바꾼 뒤의 옛 결과(staleView)는 옛 접속에 조용히 보내지 않는다
	if q == t.fetched || t.grid.FilterTyping() || t.tv != nil || t.view >= 0 || (t.running != nil && !t.requerying) || t.baseSQL == "" ||
		conn < 0 || conn != t.conn || conn != t.viewConn || m.conn(conn).Kind != db.KindPG || t.viewDB != m.conn(conn).DB || !t.grid.UniqueCols() {
		return nil
	}
	stmts := db.SplitStatements(t.baseSQL)
	if len(stmts) != 1 || !db.IsReadOnlySQL(stmts[0]) {
		return nil
	}
	if _, names := params.Bind(stmts[0]); len(names) > 0 {
		return nil
	}
	// 확인을 거절하거나 실패해도 같은 정렬·거르기로 거듭 묻지 않게 지금 것을 적는다
	t.fetched = q
	sql := t.grid.ServerSQL(stmts[0])
	sent, note, err := m.expandEnc(conn, sql)
	if err != nil {
		m.status = err.Error()
		return nil
	}
	// 읽기 판정은 첫 낱말이라 CTE 쓰기·부수효과 함수가 지날 수 있다. 정렬마다 조용히 다시 도는 실행이라 늘 READ ONLY 로 보낸다
	return m.guard(pendingRun{tab: t.id, conn: conn, readOnly: true, kind: rkGrid, tx: true, sql: sql, stmts: 1, note: note, keep: &q,
		exec: execScript(m.clients, script{conn: m.conn(conn), stmts: []string{sent}, readOnlyTx: true})})
}
