package main

import (
	"errors"
	"slices"
	"strings"

	tea "github.com/charmbracelet/bubbletea"
)

// ─── 트랜잭션 모드 ───

// commitRolledBack 은 오류 상태의 트랜잭션에 보낸 COMMIT 이 ROLLBACK 태그로 끝났을 때 알리는 글이다.
const commitRolledBack = "COMMIT 이 ROLLBACK 으로 끝남(트랜잭션 오류 상태)"

// errCommitRolledBack 은 COMMIT 을 보냈지만 서버가 ROLLBACK 으로 끝낸 것이다. 바꾼 것이 남지 않았으니 미뤄 둔 동작을 멈춘다.
var errCommitRolledBack = errors.New("COMMIT ended as ROLLBACK (transaction was aborted)")

// orphanTx 는 탭을 닫거나 접속을 바꾸면 다른 탭이 이어 쓰지 않게 되는 열린 트랜잭션의 접속이다.
// 세션은 접속마다 하나라 같은 접속을 쓰는 다른 탭이 있으면 그 탭에서 이어서 끝낼 수 있다.
func (m *model) orphanTx(t *queryTab) []int {
	if t.conn < 0 || !m.inTx[t.conn] {
		return nil
	}
	for _, o := range m.tabs {
		if o != t && o.conn == t.conn {
			return nil
		}
	}
	return []int{t.conn}
}

// askTx 는 열린 트랜잭션을 어떻게 끝낼지 묻는다. 끝내지 않고 닫거나 바꾸지 않는다.
func (m *model) askTx(p txPrompt) tea.Cmd {
	m.txAsk = &p
	m.overlay = oTx
	return nil
}

// quitAsk 는 열린 트랜잭션이 있으면 모두 어떻게 끝낼지 묻고, 없으면 끝낸다.
func (m *model) quitAsk() tea.Cmd {
	if m.unlinking() {
		return nil
	}
	var conns []int
	for c, open := range m.inTx {
		if open {
			conns = append(conns, c)
		}
	}
	if len(conns) == 0 {
		return m.quit()
	}
	slices.Sort(conns)
	return m.askTx(txPrompt{tab: m.cur().id, conns: conns, after: afterQuit})
}

// quit 은 탭을 세션에 저장하고(R41) 끝낸다. 저장 오류는 화면을 닫은 뒤 main 이 알린다.
func (m *model) quit() tea.Cmd {
	if m.unlinking() {
		return nil
	}
	m.saveErr = m.saveSession()
	m.stop() // 풀 닫기가 진행 중인 조회·실행을 기다리지 않게 모두 취소한다
	return tea.Quit
}

// unlinking 은 Redis 키 일괄 삭제(R42)가 도는 중이라 끝내지 않는지다. 끝내면 어디까지 지웠는지 알 수 없어
// esc 로 멈춰 지운 수를 본 뒤 끝내게 한다.
func (m *model) unlinking() bool {
	if m.unlink == nil {
		return false
	}
	m.status = "키를 지우는 중 — esc 로 멈춘 뒤 종료"
	return true
}

// closeAsk 는 탭 t 를 닫는다. 이 탭만 쓰던 접속의 트랜잭션이 열려 있으면 어떻게 끝낼지 먼저 묻는다(R17.3).
func (m *model) closeAsk(t *queryTab) tea.Cmd {
	if conns := m.orphanTx(t); len(conns) > 0 {
		return m.askTx(txPrompt{tab: t.id, conns: conns, after: afterClose})
	}
	m.closeTab(t)
	return nil
}

// closeTab 은 탭 t 를 닫는다. 실행 중이면 취소를 요청한다.
// 지우기 전에 끝낼 때와 같이 글을 파일로 남긴다(keepText, R45). 고친 지 1초가 안 돼 걸어 둔 저장은 탭이 없어져 버려진다.
// 사용자가 친 글을 남기지 못했으면(Redis·접속 없음·자동 저장 꺼짐·쓰기 실패) 묻고 닫는다. Ctrl+W 로 몰래 잃지 않게 한다.
func (m *model) closeTab(t *queryTab) {
	if !slices.Contains(m.tabs, t) {
		return
	}
	note := ""
	var err error
	if autoSaveOn() {
		var orig string
		switch orig, err = m.keepText(t); {
		case err != nil:
			note = "저장 실패: " + err.Error()
		case orig != "" && t.file != orig:
			note = "저장 안 한 글을 충돌 사본에 남김: " + t.file
		case orig == "" && t.file != "":
			note = "저장함: " + t.file
		}
	}
	if err != nil || t.file == "" && t.userText && strings.TrimSpace(t.editor.Value()) != "" {
		m.status = note
		m.confirm("저장하지 않은 글이 있는 탭을 닫을까요", func() tea.Cmd {
			m.dropTab(t, "")
			return nil
		})
		return
	}
	m.dropTab(t, note)
}

// dropTab 은 탭 t 를 지운다. 글은 closeTab 이 이미 남겼다. note 는 상태 줄에 앞세울 저장 알림이다.
func (m *model) dropTab(t *queryTab, note string) {
	i := slices.Index(m.tabs, t)
	if i < 0 {
		return
	}
	if t.running != nil {
		t.running()
		s := "실행 중인 탭을 닫아 취소 요청함"
		if !t.runningReadOnly {
			s += " (쓰기는 이미 반영됐을 수 있음)"
		}
		note = strings.TrimPrefix(note+" · "+s, " · ")
	}
	if t.dash != nil {
		t.dash.stop()
	}
	if t.flow != nil {
		t.flow.stop()
	}
	if note != "" {
		m.status = note
	}
	m.tabs = slices.Delete(m.tabs, i, i+1)
	m.refreshStale()
	if len(m.tabs) == 0 {
		m.newTab()
		return
	}
	// 닫은 탭은 이미 저장했으므로 switchTab(떠나는 탭 저장)을 거치지 않고 남은 탭을 고른다
	if i < m.tab || m.tab >= len(m.tabs) {
		m.tab--
	}
	m.setFocus(m.focus)
}
