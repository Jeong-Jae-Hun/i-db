package main

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"time"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/session"
)

// ─── 세션 복원 (R41) ───
// 끝낼 때 탭(제목·편집기 글·커서·연 파일·접속 uuid)과 활성 탭을 저장하고 다음에 같은 프로젝트로 켜면 되살린다.
// 결과·테이블 보기·트랜잭션·DB·스키마 고르기는 되살리지 않는다. DB_RESTORE=0 이면 탭은 저장도 복원도 하지 않는다.
// 최근 파일(R44)과 창 크기(R46.2)는 글이 아닌 설정이라 DB_RESTORE=0 이어도 같은 파일에 남기고 되살린다.

func restoreOff() bool { return os.Getenv("DB_RESTORE") == "0" }

func (m *model) panes() session.Panes { return session.Panes{Tree: m.treeW, Editor: m.edH} }

// paneSaveMsg 는 키보드로 rev 판까지 바꾼 창 크기를 저장할 시각이 됐다는 알림이다. 그사이 또 바꿨으면 버린다.
type paneSaveMsg struct{ rev int }

// queuePaneSave 는 paneSaveDelay 뒤 창 크기 저장을 건다. 키를 누르고 있어 반복 키가 와도 마지막 판만 쓴다(R46.2).
func (m *model) queuePaneSave() tea.Cmd {
	m.paneRev++
	rev := m.paneRev
	return tea.Tick(paneSaveDelay, func(time.Time) tea.Msg { return paneSaveMsg{rev: rev} })
}

// endDrag 는 경계 끌기를 끝내고 크기를 저장한다(R46.2). 끄는 중이 아니면 아무것도 하지 않는다.
func (m *model) endDrag() {
	if m.drag != bNone {
		m.drag = bNone
		m.savePanes()
	}
}

// savePanes 는 바뀐 창 크기를 세션 파일에 쓴다(R46.2). 탭은 읽어 둔 그대로 두고 끝낼 때 지금 것으로 바꾼다.
func (m *model) savePanes() {
	m.sess.Panes = m.panes()
	path, err := session.Path()
	if err == nil {
		err = session.Save(path, m.proj.Dir, m.sess)
	}
	if err != nil {
		m.status = "창 크기 저장 실패: " + err.Error()
	}
}

// saveSession 은 탭을 자동 저장한 뒤(R45) 지금 탭·최근 파일·창 크기를 세션 파일에 쓴다.
// 한 번만 쓴다(키로 끝낸 뒤 main 이 다시 부른다). DB_RESTORE=0 이면 탭은 읽어 둔 그대로 둔다.
func (m *model) saveSession() error {
	if m.saved {
		return nil
	}
	m.saved = true
	m.flushTabs()
	if restoreOff() {
		m.noteUnsaved()
	}
	path, err := session.Path()
	if err != nil {
		return err
	}
	m.sess.Panes = m.panes()
	st := m.sess
	if restoreOff() {
		return session.Save(path, m.proj.Dir, st)
	}
	st.Active, st.Tabs = m.tab, make([]session.Tab, len(m.tabs))
	for i, t := range m.tabs {
		st.Tabs[i] = session.Tab{Title: t.title, Text: t.editor.Value(), File: t.file, Cursor: t.editor.CursorOffset(),
			Name: t.name, Pinned: t.pinned}
		// 파일 탭은 저장 안 한 고침이 있을 때만 글을 두고, 되살릴 때 파일이 그사이 바뀌었는지 가를 디스크 해시를 같이 둔다
		if t.file != "" {
			if b, err := os.ReadFile(filepath.Join(m.proj.Dir, t.file)); err == nil {
				if string(b) == st.Tabs[i].Text {
					st.Tabs[i].Text = ""
				} else {
					st.Tabs[i].Disk = session.DiskHash(b)
				}
			}
		}
		if t.conn >= 0 {
			st.Tabs[i].Conn = m.conn(t.conn).UUID
		}
	}
	return session.Save(path, m.proj.Dir, st)
}

// noteUnsaved 는 DB_RESTORE=0 이라 세션에도 남지 않고 파일에도 쓰지 않은 글이 있는 탭 수를 끝낸 뒤 알린다.
// 무엇을 파일에 쓸지(R45.1)는 바꾸지 않는다. 사용자가 글을 잃은 줄 모르고 지나치지 않게 한다.
func (m *model) noteUnsaved() {
	n := 0
	for _, t := range m.tabs {
		text := t.editor.Value()
		if strings.TrimSpace(text) == "" {
			continue
		}
		if t.file != "" {
			if b, err := os.ReadFile(filepath.Join(m.proj.Dir, t.file)); err == nil && string(b) == text {
				continue
			}
		}
		n++
	}
	if n > 0 {
		m.quitNotes = append(m.quitNotes, fmt.Sprintf("%d tabs' text was not saved (DB_RESTORE=0)", n))
	}
}

// restoreSession 은 저장한 탭을 되살린다. 연 파일이 없어졌으면 글만 두고 파일 연결을 끊고, .idea 에 없는 접속은 접속 없이 연다.
// 파일 탭은 저장 안 한 글이 없으면 파일을 읽는다. 저장 안 한 글이 있는데 파일이 그사이 바뀌었으면(다른 편집기·git)
// 그 탭에는 파일 내용을 두고 저장 안 한 글은 바로 뒤 새 탭 `<이름> (저장 안 한 글)` 에 둔다. 어느 쪽도 덮어 잃지 않게 한다.
// 읽지 못하면 상태 줄에 알리고 빈 탭으로 시작한다. 되살린 탭의 자동완성 목록 읽기는 Init 이 돌린다.
// 창 크기·최근 파일은 DB_RESTORE=0 이어도 읽는다. 창 크기는 그릴 때 지금 화면에 맞춰 줄인다(R46.2).
func (m *model) restoreSession() {
	path, err := session.Path()
	var st session.State
	ok := false
	if err == nil {
		st, ok, err = session.Load(path, m.proj.Dir)
	}
	if errors.Is(err, session.ErrCorrupt) {
		m.addStatus("세션을 되살리지 못함: 파일이 깨져 " + filepath.Base(session.BadPath(path)) + " 로 옮기고 새로 시작")
		return
	}
	if err != nil {
		m.addStatus("세션을 되살리지 못함: " + err.Error())
		return
	}
	if !ok {
		return
	}
	m.sess = st
	if st.Panes.Tree > 0 {
		m.treeW = st.Panes.Tree
	}
	m.edH = st.Panes.Editor
	m.resize()
	if restoreOff() || len(st.Tabs) == 0 {
		return
	}
	m.tabs, m.tabSeq = nil, 0
	m.restoring = true
	defer func() { m.restoring = false }()
	var cmds []tea.Cmd
	var active *queryTab
	var lostFiles, lostConns, changed, dropped int
	for i, s := range st.Tabs {
		conn := -1
		if s.Conn != "" {
			if c, found := m.proj.ConnIndex(s.Conn); found {
				conn = c
			} else {
				lostConns++
			}
		}
		t := m.newTab()
		if i == st.Active {
			active = t
		}
		t.title, t.name, t.pinned = s.Title, s.Name, s.Pinned
		text := s.Text
		// 파일은 프로젝트 안의 상대 경로만 다시 잇는다. 경로를 먼저 보고 읽는다
		if s.File != "" {
			var b []byte
			err := errors.New("not a project-relative path")
			if filepath.IsLocal(s.File) {
				b, err = os.ReadFile(filepath.Join(m.proj.Dir, s.File))
			}
			switch {
			case err != nil:
				lostFiles++
			// 글이 비었으면 파일 그대로 연다. 단, 디스크가 그대로인데 글만 비었으면 사용자가 지운 것이라 빈 글로 둔다
			case s.Text == string(b) || s.Text == "" && (s.Disk == "" || s.Dropped || session.DiskHash(b) != s.Disk):
				t.file, text = s.File, string(b)
			case session.DiskHash(b) == s.Disk:
				t.file = s.File
			default:
				t.file, text = s.File, string(b)
				changed++
				u := m.newTab()
				u.title = s.Title + " (저장 안 한 글)"
				u.editor.SetValue(s.Text)
				u.editor.SetCursorOffset(s.Cursor)
				u.conflictOf = s.File // 끝낼 때 원래 파일 옆 충돌 사본에 저장한다(R45.3)
				cmds = append(cmds, m.setConn(u, conn))
			}
			// 방금 읽은 디스크 내용을 기억해 둔다. 저장 안 한 글은 디스크가 그대로일 때만 자동 저장이 쓴다(R45.2)
			if t.file != "" {
				m.stamp(t, string(b))
			}
		}
		if s.Dropped && s.File == "" { // 파일 탭은 파일에서 다시 읽었으므로 잃은 글이 아니다
			dropped++
		}
		t.editor.SetValue(text)
		t.editor.SetCursorOffset(s.Cursor)
		cmds = append(cmds, m.setConn(t, conn))
	}
	// 고정한 탭 뒤에 끼운 충돌 탭(저장 안 한 글) 때문에 고정 탭이 흩어지지 않게 다시 모은다
	slices.SortStableFunc(m.tabs, pinnedFirst)
	m.switchTab(max(0, slices.Index(m.tabs, active)))
	m.boot = tea.Batch(cmds...)
	note := fmt.Sprintf("탭 %d개 되살림", len(m.tabs))
	if changed > 0 {
		note += fmt.Sprintf(" · 파일이 바뀌어 저장 안 한 글을 새 탭에 둔 탭 %d개", changed)
	}
	if dropped > 0 {
		note += fmt.Sprintf(" · 글이 커서 비운 탭 %d개", dropped)
	}
	if lostFiles > 0 {
		note += fmt.Sprintf(" · 없어진 파일 %d개 연결 끊음", lostFiles)
	}
	if lostConns > 0 {
		note += fmt.Sprintf(" · 모르는 접속 %d개는 접속 없이 엶", lostConns)
	}
	m.addStatus(note)
}

// addStatus 는 시작 때 상태 줄에 이미 있는 알림(건너뛴 접속 등)을 덮지 않고 이어 붙인다.
func (m *model) addStatus(s string) {
	m.status = strings.TrimPrefix(m.status+" · "+s, " · ")
}
