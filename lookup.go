package main

import (
	"context"
	"errors"
	"fmt"
	"slices"
	"strconv"
	"strings"
	"sync"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/params"
	"github.com/Jeong-Jae-Hun/i-db/internal/sqlfmt"
	"github.com/Jeong-Jae-Hun/i-db/internal/statcrypt"
	"github.com/Jeong-Jae-Hun/i-db/internal/user360"
)

// ─── 사용자 한눈에 보기·다른 접속과 비교 (R51·R53) ───
// 둘 다 사람이 쓰기를 확인하지 않는 여러 접속 조회라 풀 연결의 읽기 전용 트랜잭션(RunReadOnly)으로만 돈다.
// 결과는 새 탭의 결과 작은 탭(R18)으로 보이고 실행 기록에는 남기지 않는다.

var (
	// readOnlyRun 은 접속 c 에서 문장 하나를 읽기 전용 트랜잭션으로 실행한다. 시험이 가짜로 바꿔 끼운다
	readOnlyRun = func(ctx context.Context, clients *db.Clients, c db.Conn, sql string) (*db.Result, error) {
		pg, err := clients.PG(ctx, c)
		if err != nil {
			return nil, err
		}
		return pg.RunReadOnly(ctx, sql)
	}
	// schemaSnap 은 접속 c 의 스키마 구조를 읽는다. 시험이 가짜로 바꿔 끼운다
	schemaSnap = func(ctx context.Context, clients *db.Clients, c db.Conn, schema string) (db.Snapshot, error) {
		pg, err := clients.PG(ctx, c)
		if err != nil {
			return db.Snapshot{}, err
		}
		return pg.SchemaSnapshot(ctx, schema)
	}
)

// multiMsg 는 여러 접속 조회(R51·R53.1)의 결과다. subs[i] 가 실패했으면 errs[i] 가 있다.
type multiMsg struct {
	tab, seq int
	titles   []string
	conns    []int // 오류 글에서 비밀번호를 지울 접속. 접속을 모르면 -1
	subs     []*db.Result
	errs     []error
	compare  bool // 둘째 결과를 첫째 결과 기준으로 비교해 보인다(R53.1)
}

// schemaDiffMsg 는 스키마 비교(R53.2)의 결과다.
type schemaDiffMsg struct {
	tab, seq int
	conns    [2]int
	res      *db.Result
	err      error
}

// expandEnc 는 sql 의 enc('…')를 접속 conn 의 키·IV 로 바꾼 보낼 글과 상태 줄에 붙일 글이다(R52).
// 오류에는 키·글이 들지 않는다(statcrypt 가 위치만 싣는다).
func (m *model) expandEnc(conn int, sql string) (string, string, error) {
	return expandEnc(m.proj, m.conn(conn), sql)
}

// expandEnc 는 화면(R52)과 CLI(R58)가 같이 쓰는 enc() 확장이다.
func expandEnc(proj *db.Project, c db.Conn, sql string) (string, string, error) {
	key, iv := proj.AESKeyIV(c)
	out, n, server, err := statcrypt.ExpandEnc(sql, key, iv)
	switch {
	case err != nil:
		return "", "", err
	case n == 0:
		return out, "", nil
	case server:
		return out, "키·IV 가 없어 서버 staticEncrypt 로 실행", nil
	}
	return out, fmt.Sprintf("enc() %d개 암호화", n), nil
}

// connByName 은 접속 이름의 인덱스다. 없으면 -1 이다.
func connByName(proj *db.Project, name string) int {
	return slices.IndexFunc(proj.Conns, func(c db.Conn) bool { return c.Name == name })
}

// inlineArgs 는 sql 의 $n 자리를 args[n-1] 리터럴(nil 은 NULL)로 바꾼다. RunReadOnly 가 바인딩을 받지 않아서다.
// 문자열·주석 안의 $n 은 토큰이 아니라 건드리지 않는다. 값은 QuoteLiteral 로 감싸 문장을 바꾸지 못한다.
func inlineArgs(sql string, args []any) string {
	var b strings.Builder
	last := 0
	for _, t := range sqlfmt.Tokenize(sql) {
		n, err := strconv.Atoi(strings.TrimPrefix(t.Text, "$"))
		if t.Kind != sqlfmt.Param || err != nil || n < 1 || n > len(args) {
			continue
		}
		b.WriteString(sql[last:t.Start])
		if s, ok := args[n-1].(string); ok {
			b.WriteString(db.QuoteLiteral(s))
		} else {
			b.WriteString("NULL")
		}
		last = t.End
	}
	b.WriteString(sql[last:])
	return b.String()
}

// multiErr 는 여러 접속 조회의 오류 한 줄이다. 접속을 알면 그 비밀번호를 지운다.
func (m *model) multiErr(conn int, err error) string {
	if conn < 0 {
		return oneLine(err.Error())
	}
	return m.errText(conn, err)
}

// startMulti 는 새 탭을 만들어 실행 중으로 두고, 그 탭의 id·seq 와 취소할 수 있는 컨텍스트를 돌려준다.
func (m *model) startMulti(title string) (*queryTab, context.Context) {
	t := m.newTab()
	t.title = title
	ctx, cancel := context.WithTimeout(m.life, loadTimeout)
	t.running, t.runningReadOnly, t.cancelled = cancel, true, false
	m.seq++
	t.seq = m.seq
	m.status = m.label("조회 중… (Ctrl+C 취소)")
	return t, ctx
}

// runAll 은 접속마다의 문장을 동시에 읽기 전용으로 실행한다. conns[i] 가 -1 이면 errs[i] 가 미리 채워져 있다.
func runAll(ctx context.Context, clients *db.Clients, conns []db.Conn, sqls []string, errs []error) ([]*db.Result, []error) {
	subs := make([]*db.Result, len(sqls))
	var wg sync.WaitGroup
	for i := range sqls {
		if errs[i] != nil {
			continue
		}
		wg.Add(1)
		go func() {
			defer wg.Done()
			subs[i], errs[i] = readOnlyRun(ctx, clients, conns[i], sqls[i])
		}()
	}
	wg.Wait()
	return subs, errs
}

// userBatch 는 사용자 한눈에 보기(R51)의 조회 묶음이다. idx[i] 는 접속 인덱스(모르면 -1)이고, errs[i] 가 있으면 그 조회는 돌리지 않는다.
type userBatch struct {
	titles []string
	idx    []int
	conns  []db.Conn
	sqls   []string
	errs   []error
}

// planUserLookup 은 입력 하나로 조회 묶음을 만든다. 화면(R51)과 CLI(R58)가 같이 쓴다.
// 설정을 못 읽었거나 맞는 조회가 없으면 사용자에게 보일 오류다.
func planUserLookup(proj *db.Project, input string) (userBatch, error) {
	lookups, err := user360.Load(proj.Dir)
	if err != nil {
		return userBatch{}, errors.New("사용자 조회 설정 읽기 실패: " + err.Error())
	}
	if len(lookups) == 0 {
		return userBatch{}, errors.New("사용자 조회 설정이 없음: .dbtui/user360.json")
	}
	qs := user360.Plan(lookups, input, func(name, plain string) (string, error) {
		i := connByName(proj, name)
		if i < 0 {
			return "", user360.ErrNoKey
		}
		key, iv := proj.AESKeyIV(proj.Conns[i])
		if key == "" || iv == "" {
			return "", user360.ErrNoKey
		}
		return statcrypt.Encrypt(plain, key, iv)
	})
	if len(qs) == 0 {
		return userBatch{}, errors.New("맞는 조회가 없음")
	}
	n := len(qs)
	b := userBatch{titles: make([]string, n), idx: make([]int, n), conns: make([]db.Conn, n), sqls: make([]string, n), errs: make([]error, n)}
	for i, q := range qs {
		b.titles[i] = q.Title
		ci := connByName(proj, q.Conn)
		b.idx[i] = ci
		switch {
		case q.Err != nil:
			b.errs[i] = q.Err
		case ci < 0:
			b.errs[i] = fmt.Errorf("접속 없음: %s", q.Conn)
		case proj.Conns[ci].Kind != db.KindPG:
			b.errs[i] = fmt.Errorf("PostgreSQL 접속이 아님: %s", q.Conn)
		default:
			b.conns[i], b.sqls[i] = proj.Conns[ci], inlineArgs(q.SQL, q.Args)
		}
	}
	return b, nil
}

// userLookup 은 사용자 한눈에 보기(R51)를 묻는다. 입력값은 기록·탭 제목·세션에 남기지 않는다.
func (m *model) userLookup() tea.Cmd {
	m.prompt("사용자 찾기 (이메일·uid·id·닉네임) ", "", func(input string) tea.Cmd {
		if strings.TrimSpace(input) == "" {
			m.status = "찾을 값이 없음"
			return nil
		}
		b, err := planUserLookup(m.proj, input)
		if err != nil {
			m.status = err.Error()
			return nil
		}
		t, ctx := m.startMulti("사용자 찾기")
		id, seq, clients := t.id, t.seq, m.clients
		return func() tea.Msg {
			subs, errs := runAll(ctx, clients, b.conns, b.sqls, b.errs)
			return multiMsg{tab: id, seq: seq, titles: b.titles, conns: b.idx, subs: subs, errs: errs}
		}
	})
	return nil
}

// pickOtherPG 는 conn 이 아닌 PostgreSQL 접속을 고르게 하고(커서는 stg↔prod 를 바꾼 이름) 고르면 do 를 부른다.
func (m *model) pickOtherPG(prompt string, conn int, do func(other int) tea.Cmd) {
	var names []string
	for i, c := range m.proj.Conns {
		if i != conn && c.Kind == db.KindPG {
			names = append(names, c.Name)
		}
	}
	if len(names) == 0 {
		m.status = "비교할 다른 PostgreSQL 접속이 없음"
		return
	}
	m.openPicker(prompt, names, nil, func(name string) tea.Cmd { return do(connByName(m.proj, name)) }, nil)
	// 커서는 이름의 stg·prod 를 서로 바꾼 접속이다. 둘 다 없거나 그런 접속이 없으면 첫 줄이다
	self, swapped := m.conn(conn).Name, ""
	switch {
	case strings.Contains(self, "stg"):
		swapped = strings.Replace(self, "stg", "prod", 1)
	case strings.Contains(self, "prod"):
		swapped = strings.Replace(self, "prod", "stg", 1)
	}
	m.pickerCursor = max(0, slices.Index(names, swapped))
}

// compareConns 는 커서 문장을 지금 탭 접속과 고른 접속에서 실행해 비교한다(R53.1).
func (m *model) compareConns() tea.Cmd {
	t := m.cur()
	if t.conn < 0 || m.conn(t.conn).Kind != db.KindPG {
		m.status = "비교는 PostgreSQL 접속에서만 됨"
		return nil
	}
	stmts := db.SplitStatements(m.target(t))
	switch {
	case len(stmts) != 1:
		m.status = "비교할 문장 하나에 커서를 두거나 고르세요"
		return nil
	case !db.IsReadOnlySQL(stmts[0]):
		m.status = "읽기 문장만 비교"
		return nil
	}
	if _, names := params.Bind(stmts[0]); len(names) > 0 {
		m.status = "파라미터(:name·$n)가 있는 문장은 비교하지 않음"
		return nil
	}
	base, text := t.conn, stmts[0]
	m.pickOtherPG("비교할 접속 ", base, func(other int) tea.Cmd {
		idx := []int{base, other}
		conns, sqls, errs := make([]db.Conn, 2), make([]string, 2), make([]error, 2)
		titles := make([]string, 2)
		var notes []string
		for i, c := range idx {
			conns[i], titles[i] = m.conn(c), m.conn(c).Name
			sent, note, err := m.expandEnc(c, text)
			if err != nil {
				m.status = err.Error()
				return nil
			}
			sqls[i] = sent
			if note != "" && !slices.Contains(notes, note) {
				notes = append(notes, note)
			}
		}
		nt, ctx := m.startMulti(fmt.Sprintf("비교 %s ↔ %s", titles[0], titles[1]))
		nt.conn = base
		m.status = strings.Join(append(notes, m.status), " · ")
		id, seq, clients := nt.id, nt.seq, m.clients
		return func() tea.Msg {
			subs, errs := runAll(ctx, clients, conns, sqls, errs)
			return multiMsg{tab: id, seq: seq, titles: titles, conns: idx, subs: subs, errs: errs, compare: true}
		}
	})
	return nil
}

// showMulti 는 여러 접속 조회 결과를 탭의 결과 작은 탭으로 붙인다. 실패한 조회는 오류 글 결과다.
func (m *model) showMulti(msg multiMsg) {
	t := m.findTab(msg.tab)
	if t == nil || t.seq != msg.seq {
		return
	}
	// 끝난 조회의 컨텍스트도 취소해 마감 타이머를 바로 놓는다
	if t.running != nil {
		t.running()
	}
	t.running = nil
	t.subs, t.view, t.viewConn = nil, 0, -1
	failed := 0
	for i, title := range msg.titles {
		res := msg.subs[i]
		if msg.errs[i] != nil {
			failed++
			res = &db.Result{Text: "오류: " + m.multiErr(msg.conns[i], msg.errs[i])}
		}
		t.subs = append(t.subs, subResult{title: title, res: res})
	}
	t.res = t.subs[0].res
	t.grid.SetResult(t.res)
	switch {
	case t.cancelled:
		t.summary = "취소됨"
	case msg.compare && failed > 0:
		for i, err := range msg.errs {
			if err != nil {
				t.summary = fmt.Sprintf("비교 실패 %s: %s", msg.titles[i], m.multiErr(msg.conns[i], err))
				break
			}
		}
	case msg.compare:
		// 지금 탭 접속(첫째)이 기준이다. 둘째 결과를 표에 두고 첫째에서 바뀐 것을 보인다
		t.view = 1
		t.grid.SetResult(t.subs[1].res)
		if err := t.grid.ShowDiff(t.subs[0].res); err != nil {
			t.summary = err.Error()
		} else {
			t.summary = fmt.Sprintf("%s 기준 %s 비교 (esc 닫기)", msg.titles[0], msg.titles[1])
		}
	default:
		t.summary = fmt.Sprintf("조회 %d곳", len(msg.titles))
		if failed > 0 {
			t.summary += fmt.Sprintf(" · 실패 %d", failed)
		}
	}
	m.status = t.summary
	if t != m.cur() {
		m.status = t.title + ": " + t.summary
	}
}

// schemaDiff 는 트리 접속·스키마 줄의 스키마를 다른 접속의 스키마와 비교한다(R53.2).
func (m *model) schemaDiff(n *node) tea.Cmd {
	if n == nil || (n.kind != nConn && n.kind != nSchema) || m.conn(n.conn).Kind != db.KindPG {
		m.status = "=(스키마 비교)는 PostgreSQL 접속·스키마 줄에서 누르세요"
		return nil
	}
	left, schema := n.conn, "public"
	if s := m.schemaOf[n.conn]; s != "" {
		schema = s
	}
	if n.kind == nSchema {
		schema = n.label
	}
	m.pickOtherPG("비교할 접속 ", left, func(right int) tea.Cmd {
		m.prompt(fmt.Sprintf("%s 의 스키마 ", m.conn(right).Name), schema, func(other string) tea.Cmd {
			if other == "" {
				m.status = "스키마가 없어 비교하지 않음"
				return nil
			}
			a, b := m.conn(left), m.conn(right)
			t, ctx := m.startMulti(fmt.Sprintf("스키마 %s.%s ↔ %s.%s", a.Name, schema, b.Name, other))
			id, seq, clients := t.id, t.seq, m.clients
			return func() tea.Msg {
				var sa, sb db.Snapshot
				var ea, eb error
				var wg sync.WaitGroup
				wg.Add(2)
				go func() { defer wg.Done(); sa, ea = schemaSnap(ctx, clients, a, schema) }()
				go func() { defer wg.Done(); sb, eb = schemaSnap(ctx, clients, b, other) }()
				wg.Wait()
				if err := errors.Join(ea, eb); err != nil {
					return schemaDiffMsg{tab: id, seq: seq, conns: [2]int{left, right}, err: err}
				}
				return schemaDiffMsg{tab: id, seq: seq, conns: [2]int{left, right}, res: db.DiffSnapshots(sa, sb)}
			}
		})
		return nil
	})
	return nil
}

// showSchemaDiff 는 스키마 비교 결과를 탭에 붙인다.
func (m *model) showSchemaDiff(msg schemaDiffMsg) {
	t := m.findTab(msg.tab)
	if t == nil || t.seq != msg.seq {
		return
	}
	if t.running != nil {
		t.running()
	}
	t.running, t.viewConn = nil, -1
	switch {
	case msg.err != nil:
		t.res, t.resErr, t.summary = nil, m.multiErr(msg.conns[1], errors.New(m.multiErr(msg.conns[0], msg.err))), "스키마 비교 실패"
	case len(msg.res.Rows) == 0:
		t.res, t.resErr, t.summary = msg.res, "", "스키마가 같음"
	default:
		t.res, t.resErr, t.summary = msg.res, "", fmt.Sprintf("다른 곳 %d", len(msg.res.Rows))
	}
	t.grid.SetResult(t.res)
	m.status = t.summary
	if t != m.cur() {
		m.status = t.title + ": " + t.summary
	}
}
