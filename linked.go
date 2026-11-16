package main

import (
	"cmp"
	"context"
	"fmt"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/grid"
	"github.com/Jeong-Jae-Hun/i-db/internal/tableview"
	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

// ─── 연결된 곳 찾기 (R73) ───
// 칸 값(user_id = 1315)이 든 다른 테이블을 FK 없이 이름 규칙으로 찾고, 같은 그룹·환경의 PostgreSQL 접속까지 센다.
// 찾는 값은 탭 제목·실행 기록·세션에 남기지 않는다(R51 과 같은 규칙). 상태 줄에만 한 번 보인다.

const (
	// 접속 풀이 4 연결이라 하나는 목록 읽기·테이블 보기에 남긴다
	linkedWorkers = 3
	// 같은 그룹 접속이 여럿이어도 한 번에 찾는 접속 수다. 접속마다 linkedWorkers 개씩 세므로 동시 쿼리는 최대 6개다
	linkedConnWorkers = 2
	linkedTimeout     = 20 * time.Second
	// 0행 줄이 이보다 많으면 표에서 빼고 요약에 개수만 둔다
	linkedZeroKeep = 50
)

var linkedCols = []string{"connection", "table", "column", "rows", "note"}

// linkedMenuItem 은 오른쪽 클릭 고르기 창(R71.3) 끝에 더하는 항목이다
const linkedMenuItem = "연결된 곳 찾기"

// linkedFound 는 접속 하나의 찾기 결과다. err 는 접속·후보 목록 실패다. counts[i] 가 -1 이면 세지 않았다.
type linkedFound struct {
	cols      []db.LinkedColumn
	counts    []int
	errs      []error
	truncated bool
	err       error
}

// linkedTarget 은 표 줄 하나가 가리키는 테이블 열이다. conn 은 찾은 접속 목록의 자리다.
type linkedTarget struct {
	conn int
	ref  db.TableRef
	col  string
}

// linkedView 는 연결된 곳 탭의 상태다. value 는 Enter 의 WHERE 에만 쓰고 메모리에만 둔다.
// searched 는 찾기 시작할 때 접속마다의 모습이다. Enter 는 지금 접속이 이와 같을 때만 연다(R27.1·R59.3).
// stale 은 찾은 접속 하나라도 고치거나 지웠다는 뜻이다(reloadConns).
type linkedView struct {
	res      *db.Result
	targets  map[string]linkedTarget
	conns    []int
	searched []db.Conn
	value    string
	stale    bool
}

type linkedMsg struct {
	tab, seq int
	found    []linkedFound
}

// linkedConns 는 conn 과 그룹·환경(R6.2)이 같은 PostgreSQL 접속이다. conn 이 맨 앞이고 그룹이 없으면 conn 하나다.
// 환경이 같아야 하므로 stg 에서 찾을 때 prod 는 들지 않는다.
func linkedConns(proj *db.Project, conn int) []int {
	out := []int{conn}
	cur := proj.Conns[conn]
	if cur.Group == "" {
		return out
	}
	for i, c := range proj.Conns {
		if i != conn && c.Kind == db.KindPG && c.Group == cur.Group && c.Env() == cur.Env() {
			out = append(out, i)
		}
	}
	return out
}

// linkedSearchConn 은 접속 하나에서 후보 열을 찾고 센다. 시험이 가짜로 바꿔 끼운다
var linkedSearchConn = func(ctx context.Context, clients *db.Clients, c db.Conn, col, base, value string) (f linkedFound) {
	pg, err := clients.PG(ctx, c)
	if err != nil {
		f.err = err
		return f
	}
	if f.cols, f.truncated, f.err = pg.LinkedColumns(ctx, col, base, value); f.err != nil {
		return f
	}
	f.counts, f.errs = pg.CountLinked(ctx, f.cols, value, linkedWorkers)
	return f
}

// findLinked 는 접속을 linkedConnWorkers 개씩 동시에 찾는다. 전체는 linkedTimeout 안에 끝낸다. 화면과 CLI 가 같이 쓴다.
func findLinked(ctx context.Context, clients *db.Clients, conns []db.Conn, col, base, value string) []linkedFound {
	ctx, cancel := context.WithTimeout(ctx, linkedTimeout)
	defer cancel()
	found := make([]linkedFound, len(conns))
	slots := make(chan struct{}, linkedConnWorkers)
	var wg sync.WaitGroup
	for i, c := range conns {
		wg.Add(1)
		go func() {
			defer wg.Done()
			slots <- struct{}{}
			defer func() { <-slots }()
			found[i] = linkedSearchConn(ctx, clients, c, col, base, value)
		}()
	}
	wg.Wait()
	return found
}

// sameLinkedServer 는 찾을 때의 접속 a 와 지금 접속 b 가 같은 서버·사용자·환경인지다. 데이터베이스는 호출부가 따로 본다.
// uuid 가 같아도 호스트를 고쳤으면(R59.3) 찾은 값을 다른 서버(prod 일 수도 있다)로 보내지 않는다.
func sameLinkedServer(a, b db.Conn) bool {
	return a.UUID == b.UUID && a.Kind == b.Kind && a.Host == b.Host && a.Port == b.Port && a.User == b.User && a.Env() == b.Env()
}

// linkedTable 은 찾기 결과를 표로 만든다. 줄은 접속 실패, 주인 테이블, 센 행 수 큰 순, 세지 않은 열, 0행 순이고 같으면 이름순이다.
// targets 는 표 줄의 앞 세 칸(접속·테이블·열)으로 찾는다. failed 는 접속·테이블 하나라도 실패했는지다.
func linkedTable(conns []db.Conn, found []linkedFound, errText func(i int, err error) string) (res *db.Result, targets map[string]linkedTarget, summary string, failed bool) {
	type row struct {
		rank, rows int
		cells      []string
		target     *linkedTarget
	}
	var rows []row
	var hit, zero, skipped, fails int
	var cut []string
	for i, f := range found {
		name := conns[i].Name
		if f.err != nil {
			fails++
			rows = append(rows, row{rank: -1, rows: -1, cells: []string{name, "-", "-", "-", "접속 실패: " + errText(i, f.err)}})
			continue
		}
		if f.truncated {
			cut = append(cut, name)
		}
		for j, c := range f.cols {
			r := row{rows: -1, target: &linkedTarget{conn: i, ref: c.Table, col: c.Column}}
			count, notes := "-", []string{}
			if c.Owner {
				notes = append(notes, "주인 테이블")
			}
			switch {
			case !c.Indexed:
				skipped++
				r.rank, notes = 2, append(notes, "인덱스 없음 · 안 셈")
			case f.errs[j] != nil && db.IsStatementTimeout(f.errs[j]):
				fails++
				r.rank, notes = 2, append(notes, "2초 넘어 멈춤")
			case f.errs[j] != nil:
				fails++
				r.rank, notes = 2, append(notes, "오류: "+errText(i, f.errs[j]))
			case f.counts[j] > db.LinkedCap:
				hit++
				r.rank, r.rows, count = 1, f.counts[j], fmt.Sprintf("%d+", db.LinkedCap)
			case f.counts[j] > 0:
				hit++
				r.rank, r.rows, count = 1, f.counts[j], strconv.Itoa(f.counts[j])
			default:
				zero++
				r.rank, r.rows, count = 3, 0, "0"
			}
			if c.Owner {
				r.rank = 0
			}
			r.cells = []string{name, c.Table.Schema + "." + c.Table.Table, c.Column, count, strings.Join(notes, " · ")}
			rows = append(rows, r)
		}
	}
	dropped := zero > linkedZeroKeep
	if dropped {
		rows = slices.DeleteFunc(rows, func(r row) bool { return r.rank == 3 })
	}
	slices.SortStableFunc(rows, func(a, b row) int {
		return cmp.Or(cmp.Compare(a.rank, b.rank), cmp.Compare(b.rows, a.rows),
			cmp.Compare(a.cells[0], b.cells[0]), cmp.Compare(a.cells[1], b.cells[1]), cmp.Compare(a.cells[2], b.cells[2]))
	})
	res = &db.Result{Cols: linkedCols, Types: []string{"text", "text", "text", "text", "text"}}
	targets = map[string]linkedTarget{}
	for _, r := range rows {
		res.Rows = append(res.Rows, r.cells)
		if r.target != nil {
			targets[strings.Join(r.cells[:3], "\x00")] = *r.target
		}
	}
	parts := []string{fmt.Sprintf("연결된 곳 %d", hit), fmt.Sprintf("접속 %d곳", len(conns))}
	switch {
	case dropped:
		parts = append(parts, fmt.Sprintf("0행 %d개 뺌", zero))
	case zero > 0:
		parts = append(parts, fmt.Sprintf("0행 %d", zero))
	}
	if skipped > 0 {
		parts = append(parts, fmt.Sprintf("인덱스 없어 안 셈 %d", skipped))
	}
	if fails > 0 {
		parts = append(parts, fmt.Sprintf("실패 %d", fails))
	}
	if len(cut) > 0 {
		parts = append(parts, fmt.Sprintf("후보 %d열에서 끊음: %s", db.LinkedMaxColumns, strings.Join(cut, ", ")))
	}
	return res, targets, strings.Join(parts, " · "), fails > 0
}

// linkedSearch 는 결과 칸의 Ctrl+B(R73)다. 새 탭을 실행 중으로 열고 linkedMsg 에서 표를 붙인다.
func (m *model) linkedSearch(t *queryTab, l grid.Linked) tea.Cmd {
	if m.staleView(t) {
		return nil
	}
	if t.conn < 0 || m.conn(t.conn).Kind != db.KindPG {
		m.status = "PostgreSQL 결과에서만"
		return nil
	}
	base := db.LinkedBase(l.Col, l.Table)
	if strings.EqualFold(l.Col, "id") && base == "" {
		m.status = "원본 테이블을 모르는 결과의 id 는 찾지 않음"
		return nil
	}
	idx := linkedConns(m.proj, t.conn)
	conns := make([]db.Conn, len(idx))
	for i, c := range idx {
		conns[i] = m.conn(c)
	}
	nt, ctx := m.startMulti("연결된 곳: " + l.Col)
	nt.linked = &linkedView{conns: idx, searched: conns, value: l.Value}
	m.status = fmt.Sprintf(m.label("연결된 곳 찾는 중: %s = %s · 접속 %d곳 (Ctrl+C·esc 취소)"),
		l.Col, truncate(textutil.Printable(l.Value, false), 40), len(idx))
	id, seq, clients, col, value := nt.id, nt.seq, m.clients, l.Col, l.Value
	return func() tea.Msg {
		return linkedMsg{tab: id, seq: seq, found: findLinked(ctx, clients, conns, col, base, value)}
	}
}

// showLinked 는 찾기 결과를 그 탭의 표로 붙인다. 늦게 온 이전 결과는 버린다.
func (m *model) showLinked(msg linkedMsg) {
	t := m.findTab(msg.tab)
	if t == nil || t.seq != msg.seq || t.linked == nil {
		return
	}
	if t.running != nil {
		t.running()
	}
	t.running = nil
	lv := t.linked
	res, targets, summary, _ := linkedTable(lv.searched, msg.found, func(i int, err error) string {
		return oneLine(db.Redact(err.Error(), lv.searched[i].Password))
	})
	lv.res, lv.targets = res, targets
	t.res, t.resErr, t.subs, t.view, t.viewConn = res, "", nil, -1, -1
	t.grid.SetResult(res)
	t.summary = summary + " · Enter 테이블 보기"
	if t.cancelled {
		t.summary = "취소됨 · " + summary
	}
	m.status = t.summary
	if t != m.cur() {
		m.status = t.title + ": " + t.summary
	}
}

// openLinked 는 연결된 곳 표의 커서 줄 테이블을 그 접속의 테이블 보기(R16)로 새 탭에 연다. WHERE 는 `"<열>" = <값>` 이다.
// WHERE 에 값이 들어 그 보기의 읽기는 실행 기록에 남기지 않는다(NoHistory).
func (m *model) openLinked(t *queryTab) tea.Cmd {
	lv, row := t.linked, t.grid.CurrentRow()
	if len(row) < 3 {
		return nil
	}
	tg, ok := lv.targets[strings.Join(row[:3], "\x00")]
	if !ok {
		m.status = "열 테이블이 없는 줄"
		return nil
	}
	conn, was := lv.conns[tg.conn], lv.searched[tg.conn]
	now := m.conn(conn)
	switch {
	case lv.stale || !sameLinkedServer(was, now):
		m.status = "찾은 뒤 접속을 고치거나 지워 열지 않음 (다시 찾으세요)"
		return nil
	case was.DB != now.DB:
		m.status = "찾는 사이 데이터베이스를 바꿔 열지 않음"
		return nil
	}
	where, err := db.FKWhere([]string{tg.col}, []string{lv.value})
	if err != nil {
		m.status = err.Error()
		return nil
	}
	return m.openTable(m.newTab(), conn, tg.ref, where, tableview.Options{NoHistory: true})
}
