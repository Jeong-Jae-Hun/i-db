package main

import (
	"fmt"
	"os"
	"slices"
	"strings"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/history"
	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

// ─── 결과 사본 (R11.4) ───
// 기록을 붙일 때 표·글 결과를 같이 남기고, 기록 창(Ctrl+H)에서 그 사본을 읽기 전용 탭으로 다시 연다.

// snapTab 은 사본 탭이 보이는 기록이다. r 이 그 접속에서 sql 을 다시 실행한다.
type snapTab struct {
	id, conn, sql string
}

// writeSnapshot 은 사본 쓰기다. 테스트가 막히는 쓰기로 바꿔 화면이 쓰기를 기다리지 않는지 본다
var writeSnapshot = (*history.Results).Write

// snapshotOf 는 resultMsg 의 결과 사본이다. 남기지 않는 결과면 nil 이다. 표를 바꾸는 결과(편집기·저장소 실행, 테이블 보기
// 페이지, 실행 계획)만 남기고, 비밀값을 가린 문장과 prod 결과(DB_HISTORY_PROD=1 이 아니면)는 남기지 않는다.
// 행은 앞 SnapshotRows 개만 복사한다. 쓰기는 다른 고루틴에서 하고 화면은 결과를 계속 쓰기 때문이다.
func (m *model) snapshotOf(msg resultMsg, c db.Conn, e history.Entry) *history.Snapshot {
	res := msg.res
	switch {
	case m.results == nil || m.results.Off(), msg.err != nil, res == nil:
		return nil
	case !replacesGrid[msg.kind] || msg.kind == rkEstimate, len(res.Cols) == 0 && res.Text == "":
		return nil
	case db.MaskSecrets(msg.sql) != msg.sql, c.Env() == "prod" && os.Getenv("DB_HISTORY_PROD") != "1":
		return nil
	}
	rows := make([][]string, min(len(res.Rows), history.SnapshotRows))
	for i := range rows {
		rows[i] = slices.Clone(res.Rows[i])
	}
	return &history.Snapshot{ID: e.ID, Time: e.Time, Conn: e.Conn, SQL: e.SQL, Cols: slices.Clone(res.Cols), Types: slices.Clone(res.Types),
		Rows: rows, Text: res.Text, Elapsed: res.Elapsed, RowCount: len(res.Rows), Truncated: len(res.Rows) > len(rows), ResultTruncated: res.Truncated}
}

// saveHistory 는 사본을 먼저 쓰고(성공해야 기록에 result 를 단다) 기록 한 줄을 붙이는 명령이다. 화면 밖에서 돈다.
// 사본 쓰기가 실패하면 그 실행 동안 사본을 끄고 한 번만 알린다. 기록은 그대로 붙인다.
func (m *model) saveHistory(e history.Entry, snap *history.Snapshot) tea.Cmd {
	hist, results := m.hist, m.results
	return func() tea.Msg {
		warn := ""
		if snap != nil {
			if err := writeSnapshot(results, *snap); err != nil {
				if results.Disable() {
					warn = "결과 사본을 끔: " + err.Error()
				}
			} else {
				e.Result = true
				_ = results.MaybePrune(time.Now()) // 줄이기 실패는 다음 줄이기에서 다시 한다
			}
		}
		if err := hist.Append(e); err != nil {
			return statusMsg("기록 실패: " + err.Error())
		}
		if warn != "" {
			return statusMsg(warn)
		}
		return nil
	}
}

// openResults 는 기록 파일 옆 사본 폴더를 연다. DB_HISTORY_RESULTS=0 이면 열지 않고 nil 이다.
func openResults(histPath string) (*history.Results, error) {
	if os.Getenv("DB_HISTORY_RESULTS") == "0" {
		return nil, nil
	}
	return history.OpenResults(history.ResultsDir(histPath))
}

// pruneResultsCmd 는 켤 때 오래된 사본을 줄인다(R11.4). 실패는 다음 쓰기 뒤 줄이기에서 다시 한다.
func pruneResultsCmd(r *history.Results) tea.Cmd {
	return func() tea.Msg {
		_ = r.Prune(time.Now())
		return nil
	}
}

// ─── 기록 창 (R11.2) ───

func (m *model) historyMatches() []history.Entry {
	if m.hist == nil {
		return nil
	}
	f := history.ParseQuery(m.input.Value(), time.Now())
	f.Limit = historyMax
	return m.hist.List(f)
}

func (m *model) hasSnapshot(e history.Entry) bool {
	return e.Result && m.results != nil && m.results.Has(e.ID)
}

// historyKey 는 기록 창의 키다. e·d 는 거르기 글이 없을 때만 동작이고, 있으면 글자다(Ctrl+E·Ctrl+D 는 늘 동작).
func (m *model) historyKey(k tea.KeyMsg) tea.Cmd {
	list := m.historyMatches()
	ks := k.String()
	empty := m.input.Value() == ""
	var e *history.Entry
	if m.histCursor < len(list) {
		e = &list[m.histCursor]
	}
	switch {
	case ks == "esc":
		m.overlay = oNone
		return nil
	case ks == "down" || ks == "ctrl+n":
		m.histCursor = clamp(m.histCursor+1, len(list))
		return nil
	case ks == "up" || ks == "ctrl+p":
		m.histCursor = clamp(m.histCursor-1, len(list))
		return nil
	case ks == "enter" || ks == "ctrl+e" || (ks == "e" && empty):
		m.overlay = oNone
		switch {
		case e == nil:
			return nil
		case ks == "enter" && m.hasSnapshot(*e):
			return m.openSnapshot(*e)
		}
		return m.historyToEditor(*e)
	case ks == "ctrl+d" || (ks == "d" && empty):
		switch {
		case e == nil:
		case !m.hasSnapshot(*e):
			m.status = "이 기록에는 결과 사본이 없음"
		default:
			if err := m.results.Delete(e.ID); err != nil {
				m.status = "결과 사본을 지우지 못함: " + err.Error()
				return nil
			}
			m.status = "결과 사본을 지움: " + e.Time.Local().Format("01-02 15:04")
		}
		return nil
	}
	var cmd tea.Cmd
	m.input, cmd = m.input.Update(k)
	m.histCursor = 0
	return cmd
}

// historyToEditor 는 기록의 SQL 을 편집기에 넣는다. 현재 탭이 비었으면 그 탭, 아니면 새 탭이다.
// 접속이 이미 있는 탭은 바꾸지 않는다. 다른 환경(prod 등)으로 몰래 옮기지 않기 위해서다.
func (m *model) historyToEditor(e history.Entry) tea.Cmd {
	t := m.cur()
	if t.editor.Value() != "" {
		t = m.newTab()
	}
	t.editor.SetValue(e.SQL)
	cmd := m.edited(t)
	if i := connByName(m.proj, e.Conn); t.conn < 0 && i >= 0 {
		cmd = tea.Batch(cmd, m.setConn(t, i))
	}
	m.setFocus(fEditor)
	m.status = "기록을 편집기에 넣었음"
	return cmd
}

// openSnapshot 은 기록의 사본을 새 탭 `기록 · MM-DD HH:MM` 에 읽기 전용으로 연다. 정렬·거르기·복사·내보내기는 받은 행 안에서 한다.
// 탭 접속은 기록의 접속이고(없으면 접속 없음) 편집기에는 그 SQL 이 들어간다. 사본은 1MiB 이하라 화면에서 바로 읽는다.
func (m *model) openSnapshot(e history.Entry) tea.Cmd {
	s, err := m.results.Read(e.ID)
	if err != nil {
		cmd := m.historyToEditor(e)
		m.status = "결과 사본을 읽지 못해 편집기에 넣었음: " + err.Error()
		return cmd
	}
	t := m.newTab()
	at := s.Time.Local().Format("01-02 15:04")
	t.title = "기록 · " + at
	t.editor.SetValue(s.SQL)
	cmd := m.setConn(t, connByName(m.proj, s.Conn))
	t.res = &db.Result{Cols: s.Cols, Types: s.Types, Rows: s.Rows, Text: s.Text, Elapsed: s.Elapsed, Truncated: s.Truncated || s.ResultTruncated}
	t.grid.SetResult(t.res)
	t.exportConn = -1
	t.snap = &snapTab{id: s.ID, conn: s.Conn, sql: s.SQL}
	what := fmt.Sprintf("%d rows", len(s.Rows))
	switch {
	case len(s.Cols) == 0:
		what = oneLine(s.Text)
	case s.Truncated:
		what = fmt.Sprintf("%d/%d rows (사본은 앞부분만)", len(s.Rows), s.RowCount)
	}
	t.summary = fmt.Sprintf("기록 사본 %s · %s · %s · %s · 읽기 전용 (r 다시 실행)", at, textutil.Printable(s.Conn, false), what, s.Elapsed.Round(time.Millisecond))
	m.setFocus(fResult)
	m.status = t.summary
	return cmd
}

// rerunSnapshot 은 사본 탭의 SQL 을 기록의 접속에서 보통 실행 길(prod 확인·잠금 포함)로 다시 돌린다. 결과가 오면 사본을 갈음한다.
func (m *model) rerunSnapshot(t *queryTab) tea.Cmd {
	i := connByName(m.proj, t.snap.conn)
	if i < 0 {
		m.status = "기록의 접속이 없음: " + textutil.Printable(t.snap.conn, false)
		return nil
	}
	var cat tea.Cmd
	if t.conn != i {
		cat = m.setConn(t, i)
	}
	return tea.Batch(cat, m.run(t.snap.sql))
}

// historyLine 은 기록 창의 한 줄이다. `MM-DD HH:MM · 접속 · N행 · 0.12s · SQL 첫 줄`이고 사본이 있으면 앞에 ◆, 실패면 ✗ 다.
func (m *model) historyLine(e history.Entry) string {
	mark := "  "
	switch {
	case e.Err != "":
		mark = errStyle.Render("✗ ")
	case m.hasSnapshot(e):
		mark = "◆ "
	}
	return mark + dimStyle.Render(e.Time.Local().Format("01-02 15:04")) + " · " + textutil.Printable(e.Conn, false) +
		fmt.Sprintf(" · %d행 · %.2fs · ", e.Rows, e.Elapsed.Seconds()) + textutil.Printable(oneLine(db.MaskSecrets(e.SQL)), false)
}

func (m *model) historyView() string {
	list := m.historyMatches()
	lines := []string{truncate(m.input.View(), m.w), dimStyle.Render(fitHints(m.labels(historyHints), m.w))}
	top := max(0, m.histCursor-(m.h-3))
	for i := top; i < len(list) && len(lines) < m.h; i++ {
		line := m.historyLine(list[i])
		if i == m.histCursor {
			line = lipgloss.NewStyle().Reverse(true).Render(ansi.Strip(line))
		}
		lines = append(lines, truncate(line, m.w))
	}
	return strings.Join(lines, "\n")
}
