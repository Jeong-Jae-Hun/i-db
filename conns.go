package main

import (
	"cmp"
	"context"
	"errors"
	"fmt"
	"regexp"
	"slices"
	"strconv"
	"strings"
	"time"

	"github.com/charmbracelet/bubbles/cursor"
	"github.com/charmbracelet/bubbles/textinput"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

// ─── 접속 추가·수정·삭제 (R59) ───
// db 전용 접속은 .dbtui/connections.json 에 두고 .idea 는 고치지 않는다. 저장·삭제 뒤 접속 목록을 다시 읽어 맞춘다.
// 접속 인덱스는 탭·캐시·늦게 오는 메시지가 모두 쓰므로 자리를 옮기지 않는다. 지운 접속 자리에는 빈 Conn(Kind "")을 남긴다.

const (
	cfName = iota
	cfGroup
	cfKind
	cfURL  // 읽기 전용 저장소(R60.1). DynamoDB 는 선택 엔드포인트
	cfAuth // kind 처럼 space·←→ 로 고른다
	cfHost
	cfPort
	cfDB
	cfUser
	cfPassword
	cfRegion
	cfProfile
	cfCount

	connTestTimeout = 10 * time.Second
	dataGripRefusal = "DataGrip 접속은 DataGrip 에서 고침"
)

var (
	connFieldLabels = [cfCount]string{"이름", "그룹", "종류", "URL", "인증", "호스트", "포트", "DB", "사용자", "비밀번호", "리전", "프로필"}
	// 이름 칸 끝에 붙인 접속 URL. 앞의 글은 이름으로 남긴다
	connURLRe = regexp.MustCompile(`(?i)(?:postgres|postgresql|redis|rediss|https?)://\S*$`)
	// 접속 창 종류 칸이 도는 순서
	formKinds = append([]db.Kind{db.KindPG, db.KindRedis}, db.SourceKinds...)
	// http URL 을 붙였는데 종류가 PostgreSQL·Redis 면 흔한 기본 포트로 저장소 종류를 짐작한다. 모르면 OpenSearch 다
	portKinds = map[string]db.Kind{"9200": db.KindOpenSearch, "3100": db.KindLoki, "9090": db.KindPrometheus, "3000": db.KindGrafana}
)

// connForm 은 접속 창이다. inputs[cfKind]·inputs[cfAuth] 는 쓰지 않고 kind·auth 를 space·←→ 로 바꾼다.
// 종류·인증에 따라 보이는 칸이 다르다(visible).
type connForm struct {
	edit    int // 고치는 접속 인덱스, 새 접속은 -1
	kind    db.Kind
	auth    string
	sslMode string // 칸은 없고 고치기·URL 붙여넣기에서 이어받는다(R59.1)
	inputs  [cfCount]textinput.Model
	focus   int
	status  string
	failed  bool // status 가 오류다
	testSeq int  // 시험마다 올린다. 늦게 온 이전 시험 결과를 버린다
	testing bool
	askSave bool // 시험이 실패해 저장할지 묻는 중
}

type connTestMsg struct {
	seq     int
	save    bool
	version string
	warn    string // connWarning(R60.1)
	err     error
}

// editRefusal 은 접속 c 를 고치거나 지울 수 없는 이유다(R59.3). 비면 db 전용 접속이라 된다. 화면과 CLI 가 같이 쓴다.
func editRefusal(c db.Conn) string {
	if c.Source != db.SourceDB {
		return dataGripRefusal
	}
	return ""
}

// connLabel 은 트리·팔레트·CLI 에 보일 접속 이름이다. db 전용 접속은 (db) 를 붙인다(R59.1).
func connLabel(c db.Conn) string {
	if c.Source == db.SourceDB {
		return c.Name + " (db)"
	}
	return c.Name
}

// connRoots 는 그룹 → 접속 트리다. 그룹 이름 순(그룹 없음은 맨 뒤), 그룹 안은 환경·종류·이름 순(R66.1)이고 지운 자리(Kind "")는 뺀다.
// reuse 에 있는 접속 노드(펼침·읽은 자식)와 그룹 펼침 상태는 이어받는다.
func connRoots(conns []db.Conn, reuse map[int]*node, groupOpen map[string]bool) []*node {
	byGroup := map[string]*node{}
	var roots []*node
	for i, c := range conns {
		if c.Kind == "" {
			continue
		}
		g := db.FolderOf(c)
		gn := byGroup[g]
		if gn == nil {
			open, ok := groupOpen[g]
			gn = &node{kind: nGroup, label: g, conn: -1, expanded: open || !ok, loaded: true}
			byGroup[g] = gn
			roots = append(roots, gn)
		}
		cn := reuse[i]
		if cn == nil {
			cn = &node{kind: nConn, label: c.Name, conn: i}
		}
		cn.parent = gn
		gn.children = append(gn.children, cn)
	}
	slices.SortStableFunc(roots, func(a, b *node) int {
		return cmp.Or(cmp.Compare(boolRank(a.label == db.GroupOf(db.Conn{})), boolRank(b.label == db.GroupOf(db.Conn{}))), strings.Compare(a.label, b.label))
	})
	for _, gn := range roots {
		slices.SortStableFunc(gn.children, func(a, b *node) int {
			ca, cb := conns[a.conn], conns[b.conn]
			return cmp.Or(cmp.Compare(envRank[ca.Env()], envRank[cb.Env()]), cmp.Compare(connRank(ca), connRank(cb)), strings.Compare(ca.Name, cb.Name))
		})
	}
	return roots
}

// envRank 는 폴더 안 환경 순서다(R66.1). Env 는 이 셋만 돌려준다
var envRank = map[string]int{"local": 0, "stg": 1, "prod": 2}

// connRank 는 같은 환경 안의 순서다: 쓰기 DB → 읽기 DB → Redis → 검색 → 로그·지표 → DynamoDB → aws(R66.1).
func connRank(c db.Conn) int {
	read := strings.Contains(c.Name, "-read") || strings.HasPrefix(c.Host, "read.") || strings.Contains(c.Host, ".cluster-ro-")
	switch c.Kind {
	case db.KindPG:
		return 1 + boolRank(read)
	case db.KindRedis:
		return 3 + boolRank(read)
	case db.KindOpenSearch:
		return 5
	case db.KindLoki, db.KindPrometheus, db.KindGrafana:
		return 6
	case db.KindAWS:
		return 8
	}
	return 7
}

func boolRank(b bool) int {
	if b {
		return 1
	}
	return 0
}

// ─── 창 열기 ───

// openConnForm 은 새 접속(N) 또는 고치기(E) 창을 연다. 새 접속은 고른 줄의 그룹을 채운다.
func (m *model) openConnForm(n *node, edit bool) {
	f := &connForm{edit: -1, kind: db.KindPG}
	for i := range f.inputs {
		f.inputs[i] = textinput.New()
		f.inputs[i].Prompt = ""
		f.inputs[i].CharLimit = 1024
		f.inputs[i].Width = 40
		f.inputs[i].Cursor.SetMode(cursor.CursorStatic) // 창 입력은 깜박임 메시지를 받지 않는다
	}
	f.inputs[cfPassword].EchoMode = textinput.EchoPassword
	f.inputs[cfPassword].EchoCharacter = '•'
	f.inputs[cfName].Placeholder = "app-stg2 또는 postgres://user:pw@host:5432/db"
	if edit {
		if n == nil || n.kind != nConn {
			m.status = "E(접속 고치기)는 접속 줄에서 누르세요"
			return
		}
		c := m.conn(n.conn)
		if why := editRefusal(c); why != "" {
			m.status = why
			return
		}
		f.edit, f.kind, f.auth, f.sslMode = n.conn, c.Kind, c.Auth, c.SSLMode
		for i, v := range map[int]string{cfName: c.Name, cfGroup: c.Group, cfHost: c.Host, cfDB: c.DB, cfUser: c.User,
			cfURL: c.URL, cfRegion: c.Region, cfProfile: c.Profile} {
			f.inputs[i].SetValue(v)
		}
		if c.Port > 0 {
			f.inputs[cfPort].SetValue(strconv.Itoa(c.Port))
		}
		// B 로 바꾼 데이터베이스는 메모리에만 있다. 파일에는 원래 DB 를 남긴다
		if orig, ok := m.dbOrig[n.conn]; ok {
			f.inputs[cfDB].SetValue(orig)
		}
		f.inputs[cfPassword].Placeholder = "비우면 그대로"
	} else if n != nil {
		// 고른 줄에서 위로 올라가 처음 만나는 접속의 그룹, 아니면 그룹 줄 이름(`기타` 는 그룹 없음)이다
		for p := n; p != nil; p = p.parent {
			if p.kind == nConn {
				f.inputs[cfGroup].SetValue(m.conn(p.conn).Group)
				break
			}
			if p.kind == nGroup {
				// 폴더 이름은 옮긴 폴더일 수 있어(R66.2) 자식들의 그룹이 하나일 때만 그 그룹을 채운다
				groups := map[string]bool{}
				for _, cn := range p.children {
					groups[m.conn(cn.conn).Group] = true
				}
				if len(groups) == 1 {
					for g := range groups {
						f.inputs[cfGroup].SetValue(g)
					}
				} else if len(p.children) == 0 && p.label != db.GroupOf(db.Conn{}) {
					f.inputs[cfGroup].SetValue(p.label)
				}
				break
			}
		}
	}
	f.setFocus(cfName)
	m.connForm = f
	m.overlay = oConn
}

// visible 은 지금 종류·인증에서 칸 i 를 보이는지다(R59.2·R60.1).
func (f *connForm) visible(i int) bool {
	switch i {
	case cfName, cfGroup, cfKind:
		return true
	case cfHost, cfPort, cfDB:
		return !f.kind.IsSource()
	case cfURL:
		return f.kind.IsSource() && f.kind != db.KindAWS
	case cfAuth:
		return f.kind.IsHTTP()
	case cfUser:
		return !f.kind.IsSource() || f.auth == "basic"
	case cfPassword:
		return !f.kind.IsSource() || f.auth == "basic" || f.auth == "token"
	case cfRegion, cfProfile:
		return f.kind.UsesRegion() || f.auth == "sigv4"
	}
	return false
}

// step 은 보이는 칸 사이로 d(±1)만큼 옮긴다.
func (f *connForm) step(d int) {
	i := f.focus
	for range cfCount {
		i = (i + d + cfCount) % cfCount
		if f.visible(i) {
			break
		}
	}
	f.setFocus(i)
}

// setKind 는 종류를 바꾸고 인증을 그 종류의 기본값으로 둔다.
func (f *connForm) setKind(k db.Kind) {
	f.kind, f.auth = k, ""
	if opts := db.AuthOptions(k); len(opts) > 0 {
		f.auth = opts[0]
	}
}

func (f *connForm) setFocus(i int) {
	f.focus = (i + cfCount) % cfCount
	for j := range f.inputs {
		if j == f.focus {
			f.inputs[j].Focus()
		} else {
			f.inputs[j].Blur()
		}
	}
}

// fillURL 은 이름 칸 끝의 접속 URL 을 읽어 나머지 칸을 채우고 이름 칸에는 앞의 글만 남긴다. 읽지 못하면 그대로 둔다.
// http(s) URL 은 저장소 url 칸에 넣는다(R59.2). URL 칸에 사용자 정보가 든 URL 을 붙여도 사용자·비밀번호로 나눈다.
func (f *connForm) fillURL() {
	v := f.inputs[cfName].Value()
	loc := connURLRe.FindStringIndex(v)
	if loc == nil {
		if f.focus == cfURL && strings.Contains(f.inputs[cfURL].Value(), "@") {
			f.fillHTTPURL(f.inputs[cfURL].Value())
		}
		return
	}
	if raw := v[loc[0]:]; strings.HasPrefix(strings.ToLower(raw), "http") {
		if f.fillHTTPURL(raw) {
			f.inputs[cfName].SetValue(strings.TrimSpace(v[:loc[0]]))
		}
		return
	}
	c, pw, err := db.ParseConnURL(v[loc[0]:])
	if err != nil {
		f.status, f.failed = "URL 을 읽지 못함 (postgres://user:pw@host:5432/db · redis://:pw@host:6379/0)", true
		return
	}
	f.setKind(c.Kind)
	f.sslMode = c.SSLMode
	f.inputs[cfName].SetValue(strings.TrimSpace(v[:loc[0]]))
	for i, s := range map[int]string{cfHost: c.Host, cfPort: strconv.Itoa(c.Port), cfDB: c.DB, cfUser: c.User, cfPassword: pw} {
		f.inputs[i].SetValue(s)
	}
	f.status, f.failed = "URL 로 칸을 채웠음", false
}

// fillHTTPURL 은 http(s) URL 을 url 칸과 사용자·비밀번호로 나눠 넣는다. 종류가 저장소가 아니면 포트로 짐작한다.
func (f *connForm) fillHTTPURL(raw string) bool {
	u, user, pw, err := db.ParseHTTPURL(raw)
	if err != nil {
		f.status, f.failed = "URL 을 읽지 못함 (https://host[:port])", true
		return false
	}
	if !f.kind.IsSource() {
		k, ok := portKinds[portOf(u)]
		if !ok {
			k = db.KindOpenSearch
		}
		f.setKind(k)
	}
	f.inputs[cfURL].SetValue(u)
	if user != "" || pw != "" {
		f.inputs[cfUser].SetValue(user)
		f.inputs[cfPassword].SetValue(pw)
		if slices.Contains(db.AuthOptions(f.kind), "basic") {
			f.auth = "basic"
		}
	}
	f.status, f.failed = "URL 로 칸을 채웠음", false
	return true
}

func portOf(raw string) string {
	_, rest, _ := strings.Cut(raw, "://")
	host, _, _ := strings.Cut(rest, "/")
	if i := strings.LastIndex(host, ":"); i >= 0 {
		return host[i+1:]
	}
	return ""
}

// conn 은 칸 값으로 만든 접속이다. 포트가 비면 종류의 기본 포트다. 비밀번호가 비면 그대로 비운다(고치기에서는 그대로 둠).
func (f *connForm) conn() (db.Conn, error) {
	var v [cfCount]string
	for i := range f.inputs {
		v[i] = strings.TrimSpace(f.inputs[i].Value())
	}
	if f.kind.IsSource() {
		// 보이지 않는 칸 값은 저장하지 않는다(인증을 바꾸면 이전 사용자·비밀번호가 남지 않게)
		c := db.Conn{Name: v[cfName], Group: v[cfGroup], Kind: f.kind, Auth: f.auth}
		if f.visible(cfURL) {
			c.URL = v[cfURL]
		}
		if f.visible(cfUser) {
			c.User = v[cfUser]
		}
		if f.visible(cfPassword) {
			c.Password = f.inputs[cfPassword].Value()
		}
		if f.visible(cfRegion) {
			c.Region, c.Profile = v[cfRegion], v[cfProfile]
		}
		switch {
		case f.kind.IsHTTP() && c.URL == "":
			return c, errors.New("URL 이 필요함")
		case f.visible(cfRegion) && c.Region == "":
			return c, errors.New("리전이 필요함")
		}
		return c, nil
	}
	c := db.Conn{Name: v[cfName], Group: v[cfGroup], Kind: f.kind, Host: v[cfHost], DB: v[cfDB], User: v[cfUser],
		Password: f.inputs[cfPassword].Value(), Port: db.DefaultPort(f.kind)}
	if f.kind == db.KindPG {
		c.SSLMode = f.sslMode
	}
	if p := v[cfPort]; p != "" {
		n, err := strconv.Atoi(p)
		if err != nil || n < 1 || n > 65535 {
			return c, errors.New("포트는 1~65535 숫자")
		}
		c.Port = n
	}
	if c.Host == "" {
		return c, errors.New("호스트가 필요함")
	}
	return c, nil
}

// ─── 키 ───

func (m *model) connFormKey(k tea.KeyMsg) tea.Cmd {
	f := m.connForm
	ks := k.String()
	if f.askSave {
		f.askSave = false
		if ks == "y" {
			return m.saveConnForm()
		}
		f.status = "저장하지 않음"
		return nil
	}
	switch ks {
	case "esc":
		f.testSeq++ // 진행 중인 시험 결과는 버린다
		m.overlay, m.connForm = oNone, nil
		m.status = "취소함"
		return nil
	case "tab", "down":
		if f.focus == cfName || f.focus == cfURL {
			f.fillURL()
		}
		f.step(1)
		return nil
	case "shift+tab", "up":
		f.step(-1)
		return nil
	case "ctrl+t", "enter":
		if f.testing {
			f.status = "시험 중…"
			return nil
		}
		f.fillURL()
		return m.testConnForm(ks == "enter")
	}
	// 종류·인증 칸은 space·→·l 은 다음, ←·h 는 앞 값이다
	d := map[string]int{" ": 1, "right": 1, "l": 1, "left": -1, "h": -1}[ks]
	switch f.focus {
	case cfKind:
		if d != 0 {
			port := f.inputs[cfPort].Value()
			i := slices.Index(formKinds, f.kind)
			f.setKind(formKinds[(i+d+len(formKinds))%len(formKinds)])
			// 기본 포트를 그대로 두었으면 새 종류의 기본 포트로 바꾼다
			if port == strconv.Itoa(db.DefaultPGPort) || port == strconv.Itoa(db.DefaultRedisPort) {
				f.inputs[cfPort].SetValue("")
			}
		}
		return nil
	case cfAuth:
		if opts := db.AuthOptions(f.kind); d != 0 && len(opts) > 0 {
			f.auth = opts[(slices.Index(opts, f.auth)+d+len(opts))%len(opts)]
		}
		return nil
	}
	var cmd tea.Cmd
	f.inputs[f.focus], cmd = f.inputs[f.focus].Update(k)
	if k.Paste && (f.focus == cfName || f.focus == cfURL) {
		f.fillURL()
	}
	return cmd
}

// testConnForm 은 칸 값으로 접속을 시험한다. save 면 성공 뒤 저장하고, 실패하면 저장할지 묻는다(R59.2).
func (m *model) testConnForm(save bool) tea.Cmd {
	f := m.connForm
	c, err := f.conn()
	if err == nil && save {
		err = db.ValidateConn(c)
	}
	if err != nil {
		f.status, f.failed = err.Error(), true
		return nil
	}
	if c.Password == "" && f.edit >= 0 {
		c.Password = m.conn(f.edit).Password
	}
	f.testSeq++
	seq, life, warn := f.testSeq, m.life, connWarning(c)
	f.testing, f.status, f.failed = true, "접속 시험 중…", false
	return func() tea.Msg {
		ctx, cancel := context.WithTimeout(life, connTestTimeout)
		defer cancel()
		v, err := testConn(ctx, c)
		return connTestMsg{seq: seq, save: save, version: v, warn: warn, err: err}
	}
}

func (m *model) connTested(msg connTestMsg) tea.Cmd {
	f := m.connForm
	if f == nil || msg.seq != f.testSeq {
		return nil
	}
	f.testing = false
	warn := ""
	if msg.warn != "" {
		warn = " · 경고: " + msg.warn
	}
	if msg.err != nil {
		f.status, f.failed = "접속 실패: "+msg.err.Error()+warn, true
		f.askSave = msg.save
		return nil
	}
	f.status, f.failed = "접속됨 ("+msg.version+")"+warn, false
	if msg.save {
		cmd := m.saveConnForm()
		if msg.warn != "" && m.connForm == nil { // 저장하면 창이 닫혀 경고를 상태 줄로 옮긴다
			m.status += " · 경고: " + msg.warn
		}
		return cmd
	}
	return nil
}

// connBusy 는 접속 i 를 지금 고치거나 지우면 안 되는 이유다. 비면 된다.
// 연결을 닫으면(Forget) 실행·보류 중인 편집·열린 트랜잭션이 사라지므로 먼저 끝내게 한다.
func (m *model) connBusy(i int) string {
	switch why := m.dbSwitchBlock(i); {
	case why != "":
		return why
	case m.inTx[i]:
		return "열린 트랜잭션을 먼저 끝내세요 (F10 COMMIT · F11 ROLLBACK)"
	case m.export != nil && m.export.conn == i:
		return "전체 내보내기를 먼저 끝내세요 (esc)"
	case m.unlink != nil && m.unlink.conn == i:
		return "키를 지우는 중 (esc 멈춤)"
	}
	return ""
}

func (m *model) saveConnForm() tea.Cmd {
	f := m.connForm
	c, err := f.conn()
	if err != nil {
		f.status, f.failed = err.Error(), true
		return nil
	}
	if f.edit >= 0 {
		if why := m.connBusy(f.edit); why != "" {
			f.status, f.failed = why, true
			return nil
		}
		c, err = db.UpdateConn(m.proj.Dir, m.conn(f.edit).Name, c)
	} else {
		c, err = db.AddConn(m.proj.Dir, c)
	}
	// ErrEnvNotUpdated 는 접속은 저장됐고 .env 만 못 고친 것이라 저장으로 다루고 손볼 키를 보인다
	partial := errors.Is(err, db.ErrEnvNotUpdated)
	if err != nil && !partial {
		f.status, f.failed = "저장하지 못함: "+err.Error(), true
		return nil
	}
	m.overlay, m.connForm = oNone, nil
	cmd := m.reloadConns()
	if i, ok := m.proj.ConnIndex(c.UUID); ok {
		if cn := m.connNode(i); cn != nil {
			if cn.parent != nil && !cn.parent.expanded {
				cn.parent.expanded = true
				m.rebuild()
			}
			m.selectNode(cn)
		}
	}
	m.status = strings.TrimPrefix(m.status+" · 접속 저장함: "+connLabel(c), " · ")
	if partial {
		m.status += " · " + err.Error()
	}
	return cmd
}

// ─── 지우기 ───

// removeConnAsk 는 db 전용 접속 i 를 지울지 묻는다(R59.3). DataGrip 접속은 안내만 한다.
func (m *model) removeConnAsk(i int) tea.Cmd {
	c := m.conn(i)
	if why := editRefusal(c); why != "" {
		m.status = why
		return nil
	}
	if why := m.connBusy(i); why != "" {
		m.status = why
		return nil
	}
	uuid := c.UUID
	m.confirm(fmt.Sprintf("%s 접속을 지울까요(.env 비밀번호도)", c.Name), func() tea.Cmd {
		// 묻는 사이 목록이 바뀌었을 수 있어 uuid 로 다시 찾는다
		j, ok := m.proj.ConnIndex(uuid)
		if !ok {
			m.status = "접속이 이미 없음"
			return nil
		}
		if why := m.connBusy(j); why != "" {
			m.status = why
			return nil
		}
		name := m.conn(j).Name
		if err := db.RemoveConn(m.proj.Dir, name); err != nil {
			m.status = "지우지 못함: " + err.Error()
			return nil
		}
		cmd := m.reloadConns()
		m.status = strings.TrimPrefix(m.status+" · 접속 지움: "+name, " · ")
		return cmd
	})
	return nil
}

// removeConnKey 는 팔레트 `접속 지우기`(트리 Alt+X)다. 접속 줄이면 그 접속을, 아니면 db 전용 접속을 고르게 한다(Redis 포함).
func (m *model) removeConnKey(n *node) tea.Cmd {
	if n != nil && n.kind == nConn {
		return m.removeConnAsk(n.conn)
	}
	var names []string
	for _, c := range m.proj.Conns {
		if c.Source == db.SourceDB {
			names = append(names, connLabel(c))
		}
	}
	if len(names) == 0 {
		m.status = "db 전용 접속이 없음 (N 으로 추가)"
		return nil
	}
	m.openPicker("지울 접속 ", names, nil, func(label string) tea.Cmd {
		i := connByName(m.proj, strings.TrimSuffix(label, " (db)"))
		if i < 0 {
			return nil
		}
		return m.removeConnAsk(i)
	}, nil)
	return nil
}

// ─── 폴더 (R66.2) ───

// moveFolderAsk 는 접속 줄의 M 이다. 지금 폴더를 채운 한 줄 입력을 열고, 빈 값이면 원래 그룹으로 되돌린다.
func (m *model) moveFolderAsk(n *node) {
	if n == nil || n.kind != nConn {
		m.status = "M(폴더로 옮기기)은 접속 줄에서 누르세요"
		return
	}
	i := n.conn
	c := m.conn(i)
	m.prompt("옮길 폴더: ", db.FolderOf(c), func(v string) tea.Cmd {
		if err := db.SetFolder(m.proj.Dir, []db.Conn{c}, v); err != nil {
			m.status = "폴더를 옮기지 못함: " + err.Error()
			return nil
		}
		cmd := m.reloadConns()
		if m.status != "" && m.conn(i).Folder == c.Folder {
			return cmd // 다시 읽지 못했으면 그 오류를 남긴다
		}
		if cn := m.connNode(i); cn != nil {
			m.selectNode(cn)
			m.status = "옮김: " + c.Name + " → " + db.FolderOf(m.conn(i))
		}
		return cmd
	})
}

// renameFolderAsk 는 폴더 줄의 E 다. 그 폴더에 보이는 접속을 모두 새 폴더로 옮긴다. 있는 폴더 이름이면 합쳐진다.
func (m *model) renameFolderAsk(n *node) {
	old := n.label
	var conns []db.Conn
	for _, cn := range n.children {
		conns = append(conns, m.conn(cn.conn))
	}
	m.prompt("폴더 이름: ", old, func(v string) tea.Cmd {
		v = strings.TrimSpace(v)
		if v == "" || v == old {
			return nil
		}
		if err := db.SetFolder(m.proj.Dir, conns, v); err != nil {
			m.status = "폴더 이름을 바꾸지 못함: " + err.Error()
			return nil
		}
		cmd := m.reloadConns()
		g := m.groupNode(v)
		if g == nil {
			return cmd // 다시 읽지 못했으면 그 오류를 남긴다
		}
		// 새 이름은 펼친 채로 생긴다. 접힌 폴더에 합쳤으면 옮긴 접속이 보이게 편다
		if !g.expanded {
			g.expanded = true
			m.rebuild()
		}
		m.selectNode(g)
		m.status = fmt.Sprintf("폴더 이름 바꿈: %s → %s (접속 %d)", old, v, len(conns))
		return cmd
	})
}

func (m *model) groupNode(label string) *node {
	for _, g := range m.roots {
		if g.kind == nGroup && g.label == label {
			return g
		}
	}
	return nil
}

// ─── 다시 읽기 ───

// reloadConns 는 접속 파일을 다시 읽어 uuid 로 맞춘다. 그대로인 접속은 자리·트리 노드·캐시를 두고, 새 접속은 끝에 붙인다.
// 바뀐 접속은 연결을 닫고(Forget) 캐시를 비운다. 사라진 접속 자리는 비우고 그 접속을 쓰던 탭은 접속을 잃는다.
func (m *model) reloadConns() tea.Cmd {
	p, err := db.LoadProject(m.proj.Dir)
	if err != nil {
		m.status = "접속 목록을 다시 읽지 못함: " + err.Error()
		return nil
	}
	conns := slices.Clone(m.proj.Conns)
	seen := map[int]bool{}
	var changed, removed []int
	var forget []string
	for _, c := range p.Conns {
		i := slices.IndexFunc(conns, func(o db.Conn) bool { return o.Kind != "" && o.UUID == c.UUID })
		if i < 0 {
			conns = append(conns, c)
			seen[len(conns)-1] = true
			continue
		}
		seen[i] = true
		same := c
		same.Folder = conns[i].Folder // 폴더만 옮긴 접속은 연결·캐시를 그대로 둔다(R66.3)
		// B 로 바꾼 DB 는 메모리에만 있어 비교에서 뺀다. 단 파일의 DB 자체를 고쳤으면 바뀐 접속이다(B 는 풀린다)
		if orig, ok := m.dbOrig[i]; ok && m.dbOver[i] != "" && c.DB == orig {
			same.DB = conns[i].DB
		}
		if same != conns[i] {
			forget = append(forget, conns[i].UUID)
			conns[i] = c
			changed = append(changed, i)
		}
		conns[i].Folder = c.Folder
	}
	for i, o := range conns {
		if o.Kind != "" && !seen[i] {
			forget = append(forget, o.UUID)
			conns[i] = db.Conn{}
			removed = append(removed, i)
		}
	}
	m.proj.Conns, m.proj.Mapping, m.proj.AESEnv, m.proj.Warnings = conns, p.Mapping, p.AESEnv, p.Warnings

	reuse, groupOpen := map[int]*node{}, map[string]bool{}
	var loose *node
	for _, g := range m.roots {
		if g.kind == nQueries {
			loose = g
			continue
		}
		groupOpen[g.label] = g.expanded
		for _, cn := range g.children {
			if !slices.Contains(changed, cn.conn) {
				reuse[cn.conn] = cn
			}
		}
	}
	m.roots = connRoots(conns, reuse, groupOpen)
	if loose != nil {
		m.roots = append(m.roots, loose) // refreshQueries 가 펼침 상태를 이어받고 다시 만든다
	}

	var catalogs []tea.Cmd
	for _, i := range slices.Concat(changed, removed) {
		delete(m.catalogs, i)
		m.catGen[i]++
		delete(m.enums, i)
		delete(m.schemaOf, i)
		delete(m.dbOver, i)
		delete(m.dbOrig, i)
		delete(m.inTx, i)
		delete(m.txSince, i)
		delete(m.unlocked, i)
		delete(m.patterns, i)
		for _, t := range m.tabs {
			if t.exportConn == i {
				t.exportSQL = ""
			}
			// 옛 호스트·DB 에서 낸 제출 묶음과 결과를 고친 접속(같은 인덱스)에 보내지 않는다
			t.undo = slices.DeleteFunc(t.undo, func(e *undoEntry) bool { return e.conn == i })
			if t.viewConn == i {
				t.viewDB = staleViewDB
			}
			if t.conn != i {
				continue
			}
			if slices.Contains(removed, i) {
				m.setConn(t, -1)
			} else {
				catalogs = append(catalogs, m.setConn(t, i))
			}
		}
	}
	m.refreshQueries()
	if len(p.Warnings) > 0 {
		m.status = "건너뜀: " + strings.Join(p.Warnings, ", ")
	} else {
		m.status = ""
	}
	if len(forget) == 0 {
		return tea.Batch(catalogs...)
	}
	clients, sources, life := m.clients, m.sources, m.life
	// 연결을 닫은 뒤에 자동완성 목록을 읽어야 옛 풀(옛 호스트)을 다시 쓰지 않는다
	return tea.Sequence(func() tea.Msg {
		ctx, cancel := context.WithTimeout(life, loadTimeout)
		defer cancel()
		for _, uuid := range forget {
			if err := sources.forget(uuid); err != nil {
				return statusMsg("이전 연결을 닫지 못함: " + err.Error())
			}
			if err := clients.Forget(ctx, uuid); err != nil {
				return statusMsg("이전 연결을 닫지 못함: " + err.Error())
			}
		}
		return nil
	}, tea.Batch(catalogs...))
}

// ─── 화면 ───

func (m *model) connFormView() string {
	f := m.connForm
	title := "새 접속"
	if f.edit >= 0 {
		title = "접속 고치기: " + textutil.Printable(m.conn(f.edit).Name, false)
	}
	lines := []string{helpTitle.Render(title), ""}
	for i := range cfCount {
		if !f.visible(i) {
			continue
		}
		label := connFieldLabels[i]
		switch {
		case i == cfDB && f.kind == db.KindRedis:
			label = "DB 번호"
		case i == cfPassword && f.auth == "token":
			label = "토큰"
		case i == cfURL && f.kind == db.KindDynamoDB:
			label = "엔드포인트"
		}
		var v string
		switch i {
		case cfKind, cfAuth:
			v = "‹ " + connKinds[f.kind] + " ›"
			if i == cfAuth {
				v = "‹ " + cmp.Or(f.auth, "없음") + " ›"
			}
			if f.focus == i {
				v = lipgloss.NewStyle().Reverse(true).Render(v) + dimStyle.Render("  space·←→")
			}
		case cfURL:
			if f.inputs[i].Value() == "" {
				f.inputs[i].Placeholder = "https://search.example.internal"
				if f.kind == db.KindDynamoDB {
					f.inputs[i].Placeholder = "비우면 AWS 기본"
				}
			}
			v = f.inputs[i].View()
		case cfProfile:
			if f.inputs[i].Value() == "" {
				f.inputs[i].Placeholder = "비우면 표준 체인"
			}
			v = f.inputs[i].View()
		case cfPort:
			if f.inputs[i].Value() == "" {
				f.inputs[i].Placeholder = strconv.Itoa(db.DefaultPort(f.kind))
			}
			v = f.inputs[i].View()
		default:
			v = f.inputs[i].View()
		}
		key := helpKey.Width(10).Render(label)
		if i != f.focus {
			key = dimStyle.Width(10).Render(label)
		}
		lines = append(lines, key+v)
	}
	// pgx 오류는 여러 줄이라 한 줄로 접어 창 폭에서 자른다
	status := truncate(textutil.Printable(oneLine(f.status), false), max(20, m.w-8))
	switch {
	case f.failed:
		status = errStyle.Render(status)
	case status != "":
		status = lipgloss.NewStyle().Foreground(colAccent).Render(status)
	}
	lines = append(lines, "", status)
	if f.askSave {
		lines = append(lines, confirmBar.Render(" 저장할까요? y/N "))
	}
	lines = append(lines,
		dimStyle.Render(m.label("Tab 다음 칸 · 이름 칸에 URL 붙이면 채움 · Ctrl+T 시험 · Enter 시험 뒤 저장 · esc 닫기")))
	return lipgloss.Place(m.w, m.h, lipgloss.Center, lipgloss.Center, helpBox.Render(strings.Join(lines, "\n")))
}
