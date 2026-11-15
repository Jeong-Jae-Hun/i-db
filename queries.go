package main

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"time"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/atomicfile"
	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

// ─── queries 파일: 트리 목록(R43)·최근 파일 열기(R44)·자동 저장(R45) ───

const (
	autoSaveStopped = "파일이 밖에서 바뀌어 자동 저장 멈춤 (Ctrl+S 로 덮어쓰기)"
	autoSaveOutside = "파일의 실제 경로가 프로젝트 밖이라 자동 저장 멈춤"
)

var (
	// errExternalChange 는 마지막으로 읽거나 쓴 뒤 파일이 밖에서 바뀌어(지워져) 자동 저장을 멈췄다는 뜻이다.
	errExternalChange = errors.New("file changed on disk, autosave stopped")
	// errOutside 는 심볼릭 링크를 따라간 실제 경로가 프로젝트 밖이라 쓰지 않았다는 뜻이다.
	errOutside = errors.New("real path is outside the project")
)

// autoSaveMsg 는 편집 rev 판에서 건 자동 저장 시각이 됐다는 알림이다. 그사이 더 고쳤으면 버린다.
type autoSaveMsg struct{ tab, rev int }

type queryFile struct {
	rel string // 프로젝트 상대 경로(queries/…, / 구분)
	mod time.Time
}

func autoSaveOn() bool { return os.Getenv("DB_AUTO_SAVE") != "0" }

// listQueries 는 queries/ 아래 .sql 파일을 최근 수정 순(같으면 이름 순)으로 돌려준다. 트리(R43)와 Ctrl+O(R5.1)가 같이 쓴다.
// 읽지 못하는 폴더·파일은 건너뛰고, 프로젝트 밖을 가리키거나 끊긴 심볼릭 링크는 뺀다.
// queries 폴더가 없으면 fs.ErrNotExist 를 싼 오류다.
func listQueries(dir string) ([]queryFile, error) {
	root := filepath.Join(dir, "queries")
	var files []queryFile
	err := filepath.WalkDir(root, func(p string, d fs.DirEntry, err error) error {
		switch {
		case err != nil && p == root:
			return err
		case err != nil && d != nil && d.IsDir():
			return fs.SkipDir
		case err != nil, d.IsDir(), !strings.HasSuffix(p, ".sql"):
			return nil
		}
		rel, err := filepath.Rel(dir, p)
		if err != nil {
			return nil
		}
		if d.Type()&fs.ModeSymlink != 0 {
			if _, err := realPath(dir, rel); err != nil {
				return nil
			}
		}
		info, err := os.Stat(p) // 링크면 가리키는 파일의 수정 시각
		if err != nil {
			return nil
		}
		files = append(files, queryFile{rel: filepath.ToSlash(rel), mod: info.ModTime()})
		return nil
	})
	slices.SortStableFunc(files, func(a, b queryFile) int { return b.mod.Compare(a.mod) })
	return files, err
}

// realPath 는 프로젝트 상대 경로 rel 의 심볼릭 링크를 따라간 실제 경로다. 아직 없는 파일이면 있는 부모 폴더까지 따라간다.
// 실제 경로가 프로젝트(이것도 따라간 실제 경로) 밖이면 errOutside 를 싼 오류다.
func realPath(dir, rel string) (string, error) {
	root, err := filepath.EvalSymlinks(dir)
	if err != nil {
		return "", err
	}
	p, rest := filepath.Join(dir, filepath.FromSlash(rel)), ""
	real, err := filepath.EvalSymlinks(p)
	for errors.Is(err, fs.ErrNotExist) && p != filepath.Dir(p) {
		p, rest = filepath.Dir(p), filepath.Join(filepath.Base(p), rest)
		real, err = filepath.EvalSymlinks(p)
	}
	if err != nil {
		return "", err
	}
	real = filepath.Join(real, rest)
	if r, err := filepath.Rel(root, real); err != nil || !filepath.IsLocal(r) {
		return "", fmt.Errorf("%w: %s", errOutside, rel)
	}
	return real, nil
}

// refreshQueries 는 queries 파일을 다시 읽어 접속별 목록을 만들고, 읽힌 접속 줄의 `쿼리 (N)` 과
// 트리 맨 아래 `쿼리 (접속 없음)` 을 다시 붙인다(R43). 펼침 상태와 커서 줄은 이어받는다.
func (m *model) refreshQueries() {
	m.qstale = false
	files, err := listQueries(m.proj.Dir)
	if err != nil && !errors.Is(err, fs.ErrNotExist) {
		m.status = "queries 목록 실패: " + err.Error()
	}
	m.qfiles = map[int][]string{}
	for _, f := range files {
		uuid := m.proj.Mapping[f.rel]
		i, ok := m.proj.ConnIndex(uuid)
		if !ok {
			i = -1
		}
		m.qfiles[i] = append(m.qfiles[i], f.rel)
	}

	cur := m.current()
	roots := m.roots
	var old *node
	if k := len(roots); k > 0 && roots[k-1].kind == nQueries {
		old, roots = roots[k-1], roots[:k-1]
	}
	for _, g := range roots {
		for _, cn := range g.children {
			if cn.loaded {
				m.attachQueries(cn)
			}
		}
	}
	if loose := m.qfiles[-1]; len(loose) > 0 {
		roots = append(roots, queryNode("쿼리 (접속 없음)", -1, loose, nil, old))
	}
	m.roots = roots
	m.rebuild()
	if cur == nil {
		return
	}
	// 다시 만든 묶음·파일 노드는 새 노드라 같은 파일(같은 접속의 묶음)을 찾아 커서를 둔다
	for i, r := range m.rows {
		if r.n == cur || (cur.kind == nQueries || cur.kind == nFile) && r.n.kind == cur.kind && r.n.conn == cur.conn && r.n.key == cur.key {
			m.cursor = i
			break
		}
	}
}

// attachQueries 는 접속 줄 cn 의 맨 앞에 그 접속의 쿼리 묶음을 둔다. 파일이 없으면 묶음을 뺀다.
func (m *model) attachQueries(cn *node) {
	rest := cn.children
	var old *node
	if len(rest) > 0 && rest[0].kind == nQueries {
		old, rest = rest[0], rest[1:]
	}
	if files := m.qfiles[cn.conn]; len(files) > 0 {
		rest = append([]*node{queryNode(fmt.Sprintf("쿼리 (%d)", len(files)), cn.conn, files, cn, old)}, rest...)
	}
	cn.children = rest
}

// queryNode 는 files 를 `queries/` 뒤 경로로 보이는 묶음 노드다. old 가 있으면 펼침 상태를 이어받는다.
func queryNode(label string, conn int, files []string, parent, old *node) *node {
	q := &node{kind: nQueries, label: label, conn: conn, parent: parent, loaded: true}
	if old != nil {
		q.expanded = old.expanded
	}
	for _, f := range files {
		q.children = append(q.children, &node{kind: nFile, label: strings.TrimPrefix(f, "queries/"), key: f, conn: conn, parent: q, loaded: true})
	}
	return q
}

// reusable 은 파일·결과를 열 때 새 탭 대신 그 자리에 열어도 되는 빈 탭인지다(R5.1·R44).
// 연 파일·글(공백만이면 빈 것)·결과·테이블 보기·값 편집기·보류 중인 편집이 없어야 한다.
func (t *queryTab) reusable() bool {
	return t.file == "" && strings.TrimSpace(t.editor.Value()) == "" && t.res == nil && len(t.subs) == 0 && !t.bound()
}

// openFile 은 queries 파일을 연다(Ctrl+O·트리 파일 줄·자동 열기). 이미 연 탭이 있으면 그 탭으로 간다.
// 쓰던 버퍼·결과를 덮지 않도록 현재 탭이 빈 탭(reusable)일 때만 그 탭에 연다. 매핑이 다른 접속이면
// 이 탭만 쓰던 트랜잭션을 남기지 않게 새 탭에 연다(R17.3). 포커스는 부르는 쪽이 정한다.
func (m *model) openFile(rel string) tea.Cmd {
	for i, ot := range m.tabs {
		if ot.file == rel {
			m.switchTab(i)
			m.status = "이미 열려 있음: " + rel
			return nil
		}
	}
	b, err := os.ReadFile(filepath.Join(m.proj.Dir, rel))
	if err != nil {
		m.status = "열기 실패: " + err.Error()
		return nil
	}
	t := m.cur()
	i, mapped := m.proj.ConnIndex(m.proj.Mapping[rel])
	if !t.reusable() || (mapped && i != t.conn && len(m.orphanTx(t)) > 0) {
		t = m.newTab()
	}
	t.editor.SetValue(string(b))
	t.file = rel
	m.stamp(t, string(b))
	var cmd tea.Cmd
	if mapped {
		cmd = m.setConn(t, i)
	}
	m.touch(t)
	m.status = "열었음: " + rel
	return cmd
}

// autoOpen 은 접속 conn 의 가장 최근 파일을 연다(R44). db 에서 연·실행한·저장한 기록(세션)이 먼저고,
// 없으면 그 접속에 붙은 파일 중 수정 시각이 가장 늦은 것이다. 지워진 기록은 건너뛴다.
func (m *model) autoOpen(conn int) tea.Cmd {
	if os.Getenv("DB_AUTO_OPEN") == "0" {
		return nil
	}
	for _, r := range m.sess.Recent[m.conn(conn).UUID] {
		if !filepath.IsLocal(r.File) {
			continue
		}
		if _, err := os.Stat(filepath.Join(m.proj.Dir, r.File)); err == nil {
			return m.openFile(r.File)
		}
	}
	if files := m.qfiles[conn]; len(files) > 0 {
		return m.openFile(files[0])
	}
	return nil
}

// touch 는 탭 접속의 최근 파일 맨 앞에 탭 파일을 둔다(R44). 끝낼 때·창 크기를 바꿀 때 세션에 저장된다.
func (m *model) touch(t *queryTab) {
	if t.file != "" && t.conn >= 0 {
		m.sess.Touch(m.conn(t.conn).UUID, t.file, time.Now())
	}
}

// stamp 는 방금 탭 파일에서 읽었거나 쓴 내용을 기억하고 멈춘 자동 저장을 다시 켠다.
// 자동 저장은 디스크가 아직 이 내용일 때만 덮는다(R45.2).
func (m *model) stamp(t *queryTab, disk string) {
	t.diskText, t.autoStop = disk, false
}

// edited 는 편집기 글이 바뀐 뒤 부른다. 파일에 이은 탭이면 편집 판을 올리고 autoSaveDelay 뒤 자동 저장을 건다(R45.2).
// 그사이 또 고치면 판이 달라 앞의 저장은 건너뛴다.
func (m *model) edited(t *queryTab) tea.Cmd {
	if t.file == "" || !autoSaveOn() {
		return nil
	}
	t.editRev++
	id, rev := t.id, t.editRev
	return tea.Tick(autoSaveDelay, func(time.Time) tea.Msg { return autoSaveMsg{tab: id, rev: rev} })
}

// writeTabFile 은 편집기 글을 탭 파일에 원자적으로 쓴다(R5.2). 권한은 원본을 따른다.
// 심볼릭 링크면 링크를 일반 파일로 바꿔 끼우지 않게 실제 경로에 쓰고, 실제 경로가 프로젝트 밖이면 쓰지 않는다.
func (m *model) writeTabFile(t *queryTab) error {
	p, err := realPath(m.proj.Dir, t.file)
	if err != nil {
		return err
	}
	fi, err := os.Stat(p)
	if err != nil {
		return err
	}
	text := t.editor.Value()
	if err := atomicfile.Write(p, ".db-*", []byte(text), fi.Mode().Perm()); err != nil {
		return err
	}
	m.stamp(t, text)
	m.touch(t)
	// 수정 시각이 바뀌어 트리 순서(R43)가 달라진다. 자동 저장마다 queries 를 훑지 않게 탭을 옮기거나 닫을 때·Ctrl+S 때 다시 읽는다
	m.qstale = true
	return nil
}

// refreshStale 은 자동 저장으로 쓴 파일이 있으면 queries 목록을 다시 읽는다(R43).
func (m *model) refreshStale() {
	if m.qstale {
		m.refreshQueries()
	}
}

// autoSave 는 파일에 이은 탭을 자동 저장한다(R45.2). 파일을 읽어 편집기 글과 같으면 쓰지 않고,
// 마지막으로 읽거나 쓴 내용(diskText)과 다르면 밖에서 바뀐 것이라 덮지 않고 그 탭의 자동 저장을 멈춘다(Ctrl+S 는 그래도 쓴다).
// 수정 시각·크기가 아니라 내용으로 가르므로 같은 초·같은 크기로 바뀐 것도 잡고, 내용이 같은 touch 는 멈추지 않는다.
// 단, 여기서 읽은 뒤 바꿔 끼우기 전(밀리초) 사이에 밖에서 쓴 내용은 덮는다. 잠금 없는 파일이라 남는 틈이다.
func (m *model) autoSave(t *queryTab) error {
	if t.file == "" || t.autoStop || !autoSaveOn() {
		return nil
	}
	b, err := os.ReadFile(filepath.Join(m.proj.Dir, t.file))
	switch text := t.editor.Value(); {
	case err != nil && !errors.Is(err, fs.ErrNotExist):
		return err
	case err == nil && string(b) == text:
		m.stamp(t, text)
		return nil
	case err != nil || string(b) != t.diskText:
		t.autoStop = true
		return fmt.Errorf("%w: %s", errExternalChange, t.file)
	}
	if err := m.writeTabFile(t); err != nil {
		if errors.Is(err, errOutside) {
			t.autoStop = true
		}
		return err
	}
	return nil
}

// autoSaveStatus 는 자동 저장하고 멈췄거나 실패했으면 상태 줄에 알린다. 저장하면 조용하다.
// 세션을 되살리는 중에는 하지 않는다. 되살린 탭끼리 옮겨 다니며 저장 안 한 글을 바로 쓰지 않게 한다.
func (m *model) autoSaveStatus(t *queryTab) {
	if m.restoring {
		return
	}
	switch err := m.autoSave(t); {
	case errors.Is(err, errExternalChange):
		m.status = m.label(autoSaveStopped)
	case errors.Is(err, errOutside):
		m.status = autoSaveOutside
	case err != nil:
		m.status = "자동 저장 실패: " + err.Error()
	}
}

// savableNew 는 파일 없는 탭의 글을 새 쿼리 파일로 저장할 수 있는지다(R45.1). 접속이 있고 PostgreSQL 이고 글이 있어야 한다.
// Redis 명령·저장소 질의(R60)는 SQL 파일이 아니라 세션(R41)에만 남긴다.
func (m *model) savableNew(t *queryTab) bool {
	return t.file == "" && t.conn >= 0 && m.conn(t.conn).Kind == db.KindPG && strings.TrimSpace(t.editor.Value()) != ""
}

// saveNew 는 파일 없는 탭의 글을 queries/<그룹>/<접속>-<시각>.sql 에 저장하고, 탭 접속을 매핑에 붙이고,
// 탭을 그 파일에 잇는다(R45.1). 매핑을 못 붙여도 파일에는 이어 두고 오류를 돌려준다.
func (m *model) saveNew(t *queryTab) (string, error) {
	group, name, uuid := "", "", ""
	if t.conn >= 0 {
		c := m.conn(t.conn)
		group, name, uuid = c.Group, c.Name, c.UUID
	}
	rel, err := db.UniqueQueryPath(m.proj.Dir, group, name, time.Now())
	if err != nil {
		return "", err
	}
	return rel, m.writeNew(t, rel, uuid)
}

// saveConflict 는 덮을 수 없는 글을 orig 옆 충돌 사본 <이름>-conflict-<시각>.sql 에 저장하고 탭을 그 사본에 잇는다(R45.3).
// 사본은 orig 가 붙은 접속(없으면 탭 접속)에 붙인다.
func (m *model) saveConflict(t *queryTab, orig string) (string, error) {
	rel, err := db.ConflictPath(m.proj.Dir, orig, time.Now())
	if err != nil {
		return "", err
	}
	uuid := m.proj.Mapping[orig]
	if uuid == "" && t.conn >= 0 {
		uuid = m.conn(t.conn).UUID
	}
	return rel, m.writeNew(t, rel, uuid)
}

// writeNew 는 탭 글을 새 파일 rel 에 쓰고 탭을 잇고 uuid 가 있으면 매핑에 붙인다. 파일·매핑 파일의 실제 경로가
// 프로젝트 밖이면(심볼릭 링크) 쓰지 않는다. 매핑만 실패하면 파일에는 이어 두고 오류를 돌려준다.
// 폴더는 실제 경로를 확인한 뒤 그 아래에만 만든다. 부모가 밖을 가리키는 링크여도 밖에 폴더를 만들지 않는다.
func (m *model) writeNew(t *queryTab, rel, uuid string) error {
	real, err := realPath(m.proj.Dir, rel)
	if err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(real), 0o755); err != nil {
		return err
	}
	text := t.editor.Value()
	if err := atomicfile.Write(real, ".db-*", []byte(text), 0o644); err != nil {
		return err
	}
	t.file, t.conflictOf = rel, ""
	m.stamp(t, text)
	var merr error
	if uuid != "" {
		if _, merr = realPath(m.proj.Dir, ".idea/data_source_mapping.xml"); merr == nil {
			merr = db.AddMapping(m.proj.Dir, rel, uuid)
		}
		if merr == nil {
			m.proj.Mapping[rel] = uuid
		}
	}
	m.touch(t)
	m.refreshQueries()
	if merr != nil {
		return fmt.Errorf("saved %s but mapping failed: %w", rel, merr)
	}
	return nil
}

// flushTabs 는 끝낼 때 모든 탭 글을 파일로 남긴다(keepText, R45). DB_RESTORE=0 이어도 글을 잃지 않게 한다.
// 남긴 충돌 사본과 쓰지 못한 탭은 m.quitNotes 에 적어 화면을 닫은 뒤 알린다. 세션 저장 실패와 섞지 않는다.
func (m *model) flushTabs() {
	if !autoSaveOn() {
		return
	}
	for _, t := range m.tabs {
		switch orig, err := m.keepText(t); {
		case err != nil:
			m.quitNotes = append(m.quitNotes, fmt.Sprintf("could not save %s: %v", t.title, err))
		case t.file != orig && orig != "":
			m.quitNotes = append(m.quitNotes, fmt.Sprintf("saved unsaved text of %s to %s", orig, t.file))
		}
	}
}

// keepText 는 탭을 없애기 전(끝낼 때·닫을 때) 글을 파일로 남긴다(R45). 파일 탭은 자동 저장하고, 밖에서 바뀌었거나
// 지워졌거나 실제 경로가 밖이라 자동 저장을 멈췄으면 충돌 사본에 남긴다. 파일 없는 탭은 사용자가 친 글(userText)만
// 새 쿼리로 저장하고, 되살릴 때 갈라 둔 `(저장 안 한 글)` 탭은 원래 파일의 충돌 사본에 남긴다. orig 는 원래 파일이다.
func (m *model) keepText(t *queryTab) (orig string, err error) {
	orig = t.file
	switch {
	case t.file != "":
		// autoSave 는 밖에서 바뀜·지워짐·밖 경로면 autoStop 을 켜고, 이미 멈췄으면 쓰지 않는다
		if err = m.autoSave(t); t.autoStop {
			orig, err = m.keepConflict(t)
		}
	case t.conflictOf != "" && strings.TrimSpace(t.editor.Value()) != "":
		orig = t.conflictOf
		_, err = m.saveConflict(t, orig)
	case t.userText && m.savableNew(t):
		_, err = m.saveNew(t)
	}
	return orig, err
}

// keepConflict 는 자동 저장을 멈춘 파일 탭의 글이 디스크와 다르면 충돌 사본에 남긴다. 돌려준 orig 는 원래 파일이다.
// 글이 비었거나 디스크와 같으면 잃을 글이 없어 그대로 둔다.
func (m *model) keepConflict(t *queryTab) (orig string, err error) {
	orig = t.file
	text := t.editor.Value()
	if b, rerr := os.ReadFile(filepath.Join(m.proj.Dir, orig)); rerr == nil && string(b) == text || strings.TrimSpace(text) == "" {
		return orig, nil
	}
	_, err = m.saveConflict(t, orig)
	return orig, err
}
