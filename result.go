package main

import (
	"context"
	"fmt"
	"slices"
	"strings"
	"time"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/dynamo"
	"github.com/Jeong-Jae-Hun/i-db/internal/grid"
	"github.com/Jeong-Jae-Hun/i-db/internal/opensearch"
	"github.com/Jeong-Jae-Hun/i-db/internal/params"
	"github.com/Jeong-Jae-Hun/i-db/internal/source"
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

// ─── 정렬·거르기 서버로 다시 읽기 (R14.5·R14.6) ───

// requery 는 일반 결과의 정렬·거르기가 지금 결과를 만든 것(t.fetched)과 달라졌으면 서버에서 다시 읽는다.
// 거르기 글을 치는 동안·테이블 보기·바뀐 것이 없거나 마지막으로 보낸(묻은) 것(t.asked)과 같으면 아무것도 하지 않는다. 다시 읽을 수 없으면 받은 행 안의 결과가 남고
// 같은 정렬·거르기에 한 번만 `받은 행만 정렬: <이유>` 를 알린다(R14.6). 키마다 불리므로 같은 알림을 거듭 덮어쓰지 않는다.
func (m *model) requery(t *queryTab) tea.Cmd {
	q := t.grid.Query()
	if t.grid.FilterTyping() || t.tv != nil || q == t.fetched || q == t.asked {
		return nil
	}
	sorting := q.SortCol != t.fetched.SortCol || q.Desc != t.fetched.Desc
	cmd, reason := m.requeryCmd(t, q, sorting)
	if reason != "" && q != t.noted {
		t.noted = q
		m.status = "받은 행만 거르기: " + reason
		if sorting {
			m.status = "받은 행만 정렬: " + reason
		}
	}
	return cmd
}

// requeryCmd 는 접속 종류마다 다시 읽는 실행이다. 보낼 것이 없으면 nil 이고, 다시 읽을 수 없으면 까닭이 붙는다.
func (m *model) requeryCmd(t *queryTab, q grid.Query, sorting bool) (tea.Cmd, string) {
	// 돌고 있는 것이 다시 읽기면 start 가 그것을 취소하고 새로 보낸다
	conn, reason := m.rereadConn(t, t.requerying)
	if reason != "" {
		return nil, reason
	}
	switch c := m.conn(conn); {
	case c.Kind == db.KindPG:
		return m.requerySQL(t, conn, q)
	case c.Kind == db.KindOpenSearch:
		return m.requerySearch(t, conn, q)
	case !sorting:
		return nil, "이 저장소는 서버 거르기 없음"
	case c.Kind == db.KindLoki || c.Kind == db.KindGrafana:
		return m.requeryLogs(t, conn, q)
	case c.Kind == db.KindDynamoDB:
		return m.requeryDynamo(t, conn, q)
	}
	return nil, "이 저장소는 서버 정렬 없음"
}

// rereadConn 은 지금 결과를 서버에서 다시(더) 읽을 접속이다. 고정·문장별 결과를 보고 있거나, 다른 실행이 돌거나
// (busyOK 면 돌아도 됨), 탭 접속을 바꾼 뒤의 옛 결과(staleView)면 까닭을 돌려준다. 옛 접속에 조용히 보내지 않는다.
func (m *model) rereadConn(t *queryTab, busyOK bool) (int, string) {
	conn := t.exportConn
	switch {
	case t.view >= 0:
		return conn, "고정·문장별 결과"
	case t.running != nil && !busyOK:
		return conn, "다른 실행 중"
	case conn < 0 || conn != t.conn || conn != t.viewConn || t.viewDB != m.conn(conn).DB:
		return conn, "접속이 바뀜"
	}
	return conn, ""
}

// pgBase 는 PostgreSQL 결과를 감싸 다시 읽을 원래 문장이다. R33.1 F7 과 같은 기준(문장 하나·읽기·파라미터 없음)에
// 열 이름이 겹치지 않아야 한다. 아니면 까닭을 돌려준다. 더 읽기(R14.7)도 같은 기준이다.
func pgBase(t *queryTab) (string, string) {
	if t.baseSQL == "" {
		return "", "편집기 실행 결과가 아님"
	}
	stmts := db.SplitStatements(t.baseSQL)
	switch {
	case len(stmts) != 1:
		return "", "문장이 둘 이상"
	case !db.IsReadOnlySQL(stmts[0]):
		return "", "쓰기 문장"
	case !t.grid.UniqueCols():
		return "", "열 이름 겹침"
	}
	if _, names := params.Bind(stmts[0]); len(names) > 0 {
		return "", "파라미터 문장"
	}
	return stmts[0], ""
}

// requerySQL 은 PostgreSQL 원래 문장을 감싸 정렬·거르기를 서버에서 한다(R14.5).
func (m *model) requerySQL(t *queryTab, conn int, q grid.Query) (tea.Cmd, string) {
	base, reason := pgBase(t)
	if reason != "" {
		return nil, reason
	}
	// 확인을 거절하거나 실패해도 같은 정렬·거르기로 거듭 묻지 않게 보낸 것만 적는다. t.fetched 는 성공한 결과가 붙을 때 바뀐다
	t.asked = q
	sql := t.grid.ServerSQL(base)
	sent, note, err := m.expandEnc(conn, sql)
	if err != nil {
		m.status = err.Error()
		return nil, ""
	}
	// 읽기 판정은 첫 낱말이라 CTE 쓰기·부수효과 함수가 지날 수 있다. 정렬마다 조용히 다시 도는 실행이라 늘 READ ONLY 로 보낸다
	return m.guard(pendingRun{tab: t.id, conn: conn, readOnly: true, kind: rkGrid, tx: true, sql: sql, stmts: 1, note: note, keep: &q,
		exec: execScript(m.clients, script{conn: m.conn(conn), stmts: []string{sent}, readOnlyTx: true})}), ""
}

// requerySearch 는 OpenSearch `_search` 결과의 정렬·거르기를 본문 sort·query 로 바꿔 서버에서 다시 읽는다(R61.3).
// 둘 다 풀면 원래 요청을 보낸다.
func (m *model) requerySearch(t *queryTab, conn int, q grid.Query) (tea.Cmd, string) {
	switch {
	case t.baseSrc == nil:
		return nil, "편집기 실행 결과가 아님"
	case db.FirstSQLWord(t.baseSrc.Text) == "select":
		return nil, "SELECT 결과"
	case !opensearch.IsSearch(t.baseSrc.Text):
		return nil, "_search 결과가 아님"
	}
	r := opensearch.Rewrite{Sort: q.SortCol, Desc: q.Desc}
	if q.Filter != "" {
		r.Filter = searchFilter(grid.ParseFilter(q.Filter, t.res.Cols))
	}
	t.asked = q
	if r.Sort == "" && r.Filter == nil {
		return m.sourceRequery(t, conn, q, *t.baseSrc, sourceRun, nil), ""
	}
	sent := new(string)
	return m.sourceRequery(t, conn, q, *t.baseSrc, func(sq source.Query) func(context.Context, source.Backend) (*db.Result, error) {
		return func(ctx context.Context, b source.Backend) (*db.Result, error) {
			text, res, err := opensearch.Requery(ctx, b, sq, r)
			*sent = text
			return res, err
		}
	}, sent), ""
}

// searchOps 는 R14.4 거르기 연산을 opensearch.Filter 의 Op 글로 바꾼다.
var searchOps = map[grid.FilterOp]string{grid.OpContains: ":", grid.OpNotContains: "!:", grid.OpEq: "=", grid.OpNe: "!=",
	grid.OpGt: ">", grid.OpGe: ">=", grid.OpLt: "<", grid.OpLe: "<=", grid.OpNull: "null", grid.OpNotNull: "notnull"}

func searchFilter(f grid.Filter) *opensearch.Filter {
	return &opensearch.Filter{Field: f.Name, Op: searchOps[f.Op], Value: f.Value}
}

// requeryLogs 는 Loki 로그 결과(time·라벨·line)의 time 정렬을 질의 방향으로 바꿔 다시 읽는다(R62.3).
// 오름차순은 forward(범위의 오래된 줄부터), 내림차순·정렬 없음은 원래 방향 backward 다. 범위(since)는 원래 실행 그대로다.
// 방향이 지금 결과와 같으면 받은 행이 이미 그 줄들이라 보내지 않는다.
func (m *model) requeryLogs(t *queryTab, conn int, q grid.Query) (tea.Cmd, string) {
	cols := t.res.Cols
	switch {
	case t.baseSrc == nil:
		return nil, "편집기 실행 결과가 아님"
	case len(cols) < 2 || cols[0] != "time" || cols[len(cols)-1] != "line":
		return nil, "로그 결과가 아님"
	case q.SortCol != "" && q.SortCol != "time":
		return nil, "time 열만 서버 정렬"
	}
	if logDirection(q) == logDirection(t.fetched) {
		return nil, ""
	}
	t.asked = q
	sq := *t.baseSrc
	sq.Direction = logDirection(q)
	return m.sourceRequery(t, conn, q, sq, sourceRun, nil), ""
}

func logDirection(q grid.Query) string {
	if q.SortCol == "time" && !q.Desc {
		return "forward"
	}
	return "backward"
}

// requeryDynamo 는 PartiQL 결과를 테이블(인덱스) 정렬 키로 정렬하면 `ORDER BY "<정렬 키>"` 를 붙여 다시 읽는다(R63.3).
// 파티션 키 = 조건이 있어야 하고 키 이름은 열린 Backend 가 Run 때 읽어 둔 것을 쓴다. 정렬을 풀면 원래 문장을 보낸다.
func (m *model) requeryDynamo(t *queryTab, conn int, q grid.Query) (tea.Cmd, string) {
	if t.baseSrc == nil {
		return nil, "편집기 실행 결과가 아님"
	}
	sq := *t.baseSrc
	if q.SortCol != "" {
		kb, ok := m.sources.peek(m.conn(conn)).(interface{ SortKey(string) (string, string) })
		if !ok {
			return nil, "키 정보 없음"
		}
		sk, reason := kb.SortKey(sq.Text)
		switch {
		case reason != "":
			return nil, reason
		case q.SortCol != sk:
			return nil, "정렬 키(" + sk + ")가 아님"
		}
		if sq.Text, ok = dynamo.Sorted(sq.Text, sk, q.Desc); !ok {
			return nil, "LIMIT 이 있는 문장"
		}
	} else if t.fetched.SortCol == "" {
		return nil, ""
	}
	t.asked = q
	return m.sourceRequery(t, conn, q, sq, sourceRun, nil), ""
}

// sourceRequery 는 저장소 다시 읽기 실행이다. 원래 요청을 비싼 질의 확인 뒤 보냈어도 다시 읽기는 같은 확인을 다시 받는다
// (runSource 와 같다). run 은 Force 를 바꾼 질의마다 실행을 만들고, sent 가 있으면 실행이 실제로 보낸 글을 기록에 싣는다.
func (m *model) sourceRequery(t *queryTab, conn int, keep grid.Query, sq source.Query,
	run func(source.Query) func(context.Context, source.Backend) (*db.Result, error), sent *string) tea.Cmd {
	c := m.conn(conn)
	sq.Force = false
	p := pendingRun{tab: t.id, conn: conn, readOnly: true, kind: rkGrid, sql: sq.Text, stmts: 1, keep: &keep, sent: sent,
		exec: m.sourceExec(c, run(sq))}
	forced := sq
	forced.Force = true
	retry := p
	retry.exec = m.sourceExec(c, run(forced))
	p.retry = &retry
	return m.guard(p)
}

// ─── 더 읽기 (R14.7) ───

// maxMoreRows 는 더 읽기로 모으는 행 상한이다. 결과는 모든 칸을 문자열로 들고(칸마다 64KiB까지) 거르기용 소문자 사본도
// 만들어 1만 행에서 막는다. 그 이상은 F7 전체 내보내기로 받는다. 늘리려면 보이는 창 근처만 들고 나머지는 서버 커서로 읽어야 한다
const maxMoreRows = 10 * db.MaxRows

// loadMore 는 잘린 PostgreSQL 일반 결과(R14.5 와 같은 기준)의 다음 1,000행을 지금 정렬·거르기로 읽어 덧붙인다(`]`).
func (m *model) loadMore(t *queryTab) tea.Cmd {
	conn, reason := m.rereadConn(t, false)
	switch {
	case t.res == nil || len(t.res.Cols) == 0:
		reason = "더 읽을 표가 없음"
	case reason != "":
	case m.conn(conn).Kind != db.KindPG:
		reason = "PostgreSQL 결과만 더 읽음"
	case !t.res.Truncated:
		reason = "더 읽을 행이 없음"
	case len(t.res.Rows) >= maxMoreRows:
		reason = fmt.Sprintf("%s행까지만 더 읽음 (F7 전체 내보내기)", textutil.GroupDigits(maxMoreRows))
	case t.grid.Pending() > 0:
		m.status = fmt.Sprintf(m.label("변경 %d건을 먼저 제출(Ctrl+S)하거나 되돌리세요(u)"), t.grid.Pending())
		return nil
	case t.grid.Query() != t.fetched:
		reason = "정렬·거르기가 받은 행에만 걸림"
	}
	base := ""
	if reason == "" {
		base, reason = pgBase(t)
	}
	if reason != "" {
		m.status = "더 읽지 않음: " + reason
		return nil
	}
	sql := t.grid.PageSQL(base, len(t.res.Rows))
	sent, note, err := m.expandEnc(conn, sql)
	if err != nil {
		m.status = err.Error()
		return nil
	}
	return m.guard(pendingRun{tab: t.id, conn: conn, readOnly: true, kind: rkMore, tx: true, sql: sql, stmts: 1, note: note,
		exec: execScript(m.clients, script{conn: m.conn(conn), stmts: []string{sent}, readOnlyTx: true})})
}

// appendMore 는 더 읽은 행을 결과에 덧붙이고 요약(`2,000행 · 더 있음`)을 돌려준다. 고정한 결과가 같은 행을 가리킬 수 있어
// 결과를 복사해 바꾼다. 읽는 사이 정렬·거르기를 받은 행에서 바꿨으면 첫 페이지부터 다시 읽는다.
func (m *model) appendMore(t *queryTab, res *db.Result) (string, tea.Cmd) {
	if t.res == nil || res == nil || len(res.Cols) != len(t.res.Cols) {
		return "더 읽은 결과의 열이 달라 붙이지 않음", nil
	}
	if n := t.grid.Pending(); n > 0 {
		return fmt.Sprintf("실행 중에 고친 변경 %d건이 있어 더 읽은 행을 붙이지 않음", n), nil
	}
	rows, more := res.Rows, res.Truncated
	if room := maxMoreRows - len(t.res.Rows); len(rows) > room {
		rows, more = rows[:room], true
	}
	nr := *t.res
	nr.Rows, nr.Truncated = append(slices.Clip(t.res.Rows), rows...), more
	t.res = &nr
	if t.view < 0 {
		t.grid.Extend(&nr)
	}
	t.summary = textutil.GroupDigits(int64(len(nr.Rows))) + "행"
	if more {
		t.summary += " · 더 있음"
	}
	return t.summary, m.requery(t)
}
