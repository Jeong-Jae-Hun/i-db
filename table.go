package main

import (
	"context"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"

	"github.com/Jeong-Jae-Hun/i-db/internal/tableview"
)

// ─── 테이블 보기 ───

// tablePage 는 테이블 보기의 지금 페이지를 읽는 실행이다. 기록에는 enc() 를 바꾸기 전 SQL 이 남는다(R52).
// 탭 접속이 바뀌었어도 테이블 보기를 연 접속에서 읽는다. NoHistory 보기(R73.5)는 sql 을 비워 기록하지 않는다.
func (m *model) tablePage(t *queryTab) pendingRun {
	q := t.tv.SQL()
	p := pendingRun{tab: t.id, conn: t.viewConn, readOnly: true, kind: rkTable, tx: true, sql: q, stmts: 1,
		exec: m.encExec(t.viewConn, q)}
	if t.tv.NoHistory() {
		p.sql = ""
	}
	return p
}

// encExec 는 테이블 보기 문장 q 의 WHERE 에 든 enc('…')를 바꿔 실행한다(R52). 바꿀 수 없으면 실행하지 않고
// 그 오류를 결과로 돌려준다. 테이블 보기는 여러 곳(페이지·다시 읽기·제출 뒤)에서 다시 읽어 오류를 결과 화면에 둔다.
func (m *model) encExec(conn int, q string) func(ctx context.Context) (*db.Result, error) {
	sent, _, err := m.expandEnc(conn, q)
	if err != nil {
		return func(context.Context) (*db.Result, error) { return nil, err }
	}
	// WHERE 는 사용자 글이라 쓰기 함수가 들 수 있다. 잠긴 prod 면 READ ONLY 트랜잭션으로 읽는다(R54)
	return execScript(m.clients, script{conn: m.conn(conn), stmts: []string{sent}, readOnlyTx: m.prodLocked(conn)})
}

// tableKey 는 테이블 보기 키(WHERE·ORDER BY·페이지·개수)를 먼저 보고, 쓰지 않은 키는 결과 표로 넘긴다.
// s 는 받은 페이지만 정렬하지 않고 커서 열 머리 클릭처럼 서버 ORDER BY 로 다시 읽는다(R14.3).
// 레코드·값 보기나 사각형 선택 중이면 esc·s·/ 는 그 화면이 쓴다.
func (m *model) tableKey(k tea.KeyMsg) tea.Cmd {
	t, ks := m.cur(), k.String()
	switch {
	case t.tv.Typing():
	case t.grid.Viewing() && (ks == "esc" || ks == "s" || ks == "/"):
		return m.gridKey(k)
	case ks == "s":
		col := t.grid.CurrentCol()
		if col == "" { // 아직 결과가 없으면 정렬할 열을 모른다. 빈 이름으로 ORDER BY 를 만들지 않는다
			m.status = "정렬할 열이 없음"
			return nil
		}
		return m.tableAction(t.tv.HeaderClick(col))
	}
	where := t.tv.TypingWhere() && ks == "enter"
	text := t.tv.Input()
	tv, a := t.tv.Update(k)
	if !a.Handled {
		return m.gridKey(k)
	}
	if where && !tv.Typing() {
		m.noteWhere(t, text) // 적용한 WHERE 입력을 기록한다(R75.5, prod 는 남기지 않음)
	}
	return m.tableAction(tv, a)
}

// tableAction 은 테이블 보기의 요청을 실행으로 바꾼다. 다시 읽기는 보류 중인 편집을 버릴지 먼저 묻고,
// 거절하면 바뀌기 전 페이지·정렬로 되돌려 화면과 상태가 어긋나지 않게 한다.
func (m *model) tableAction(tv tableview.Model, a tableview.Action) tea.Cmd {
	t := m.cur()
	if a.Status != "" {
		m.status = a.Status
	}
	if (a.Requery || a.Count) && m.staleView(t) {
		return nil
	}
	prev := *t.tv
	*t.tv = tv
	switch {
	case a.Requery:
		p := m.tablePage(t)
		p.restore = &prev
		return m.guard(p)
	case a.Count:
		return m.start(pendingRun{tab: t.id, conn: t.viewConn, readOnly: true, kind: rkCount, tx: true,
			exec: m.encExec(t.viewConn, tv.CountSQL())})
	}
	return nil
}
