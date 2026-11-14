package main

import (
	"fmt"
	"os"
	"slices"
	"strconv"
	"strings"
	"time"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

// ─── prod 쓰기 잠금 (R54) ───
// prod 접속은 켤 때 쓰기가 잠겨 있고, Ctrl+L 로 활성 탭 접속만 DB_PROD_UNLOCK_MIN 분 연다.
// 잠금은 확인(R6·R25)을 대신하지 않고 그 앞에 하나 더 놓는 문이다. 세션에 저장하지 않아 켤 때마다 다시 잠긴다.

const (
	defaultUnlockMin = 10
	maxUnlockMin     = 24 * 60
)

var (
	// lockNow 는 잠금 시계다. 시험이 바꿔 끼운다
	lockNow = time.Now
	// lockTick 은 헤더의 남은 시간을 다시 그리는 간격이다. 시험이 줄인다
	lockTick = time.Second
)

// lockTickMsg 는 열린 잠금의 남은 시간을 다시 그리고 다 된 잠금을 닫게 한다.
type lockTickMsg struct{}

// unlockMinutes 는 잠금을 여는 시간(분)이다. 양의 정수가 아니면 기본값이고, 하루(maxUnlockMin)를 넘으면 하루로 줄인다.
// 큰 값으로 잠금을 사실상 끄지 못하게 하고 time.Duration 이 넘치지 않게 한다.
func unlockMinutes() int {
	if n, err := strconv.Atoi(os.Getenv("DB_PROD_UNLOCK_MIN")); err == nil && n > 0 {
		return min(n, maxUnlockMin)
	}
	return defaultUnlockMin
}

// prodLocked 는 접속 conn 이 잠긴 prod 인지다. 연 시간이 지났으면 잠긴 것으로 본다.
func (m *model) prodLocked(conn int) bool {
	if !m.prodLock || conn < 0 || m.conn(conn).Env() != "prod" {
		return false
	}
	return !lockNow().Before(m.unlocked[conn])
}

// lockRefuse 는 잠긴 prod 에 쓰려 하면 안내하고 참이다. 쓰기 경로마다 실행·확인 창 앞에서 부른다.
func (m *model) lockRefuse(conn int) bool {
	if !m.prodLocked(conn) {
		return false
	}
	m.status = fmt.Sprintf("prod(%s) 쓰기 잠김 — %s 로 %d분 열기", m.conn(conn).Name, m.label("Ctrl+L"), unlockMinutes())
	return true
}

// txEndWords 는 열린 트랜잭션을 끝내기만 하는 문장의 첫 단어다
var txEndWords = map[string]bool{"commit": true, "rollback": true, "end": true, "abort": true}

// writeLocked 는 실행 p 가 잠긴 prod 에 쓰는지다. COMMIT·ROLLBACK 은 연 사이 시작한 트랜잭션을 끝낼 수 있게 막지 않는다.
// F10/F11 뿐 아니라 편집기에 친 commit·rollback·end·abort 만으로 된 실행도 같다. COMMIT PREPARED·ROLLBACK PREPARED 는
// 다른 준비된 트랜잭션을 확정하는 쓰기라 막는다. 편집기의 begin·set 은 읽기 전용 판정이 아니면 그대로 막는다.
func (m *model) writeLocked(p *pendingRun) bool {
	if p.readOnly || p.kind == rkTx {
		return false
	}
	editorPG := p.kind == rkGrid && p.conn >= 0 && m.conn(p.conn).Kind == db.KindPG
	if stmts := db.SplitStatements(p.sql); editorPG && len(stmts) > 0 && !slices.ContainsFunc(stmts, func(st string) bool {
		return !txEndWords[db.FirstSQLWord(st)] || slices.Contains(strings.Fields(strings.ToLower(db.MaskSQL(st))), "prepared")
	}) {
		return false
	}
	if !m.lockRefuse(p.conn) {
		return false
	}
	if w := db.FirstSQLWord(p.sql); editorPG && (m.inTx[p.conn] || w == "begin" || w == "start" || w == "set") {
		m.status += " · 트랜잭션 끝내기는 F10/F11"
	}
	return true
}

// toggleLock 은 활성 탭 접속의 잠금을 열거나 닫는다(Ctrl+L).
func (m *model) toggleLock() tea.Cmd {
	t := m.cur()
	if t.conn < 0 || m.conn(t.conn).Env() != "prod" {
		m.status = "prod 접속이 아니라 잠그지 않음"
		return nil
	}
	name := m.conn(t.conn).Name
	if !m.prodLocked(t.conn) {
		delete(m.unlocked, t.conn)
		m.status = fmt.Sprintf("prod(%s) 쓰기 잠금", name)
		return nil
	}
	n := unlockMinutes()
	m.unlocked[t.conn] = lockNow().Add(time.Duration(n) * time.Minute)
	m.status = fmt.Sprintf("prod(%s) 쓰기 %d분 열림 (확인은 그대로, %s 로 바로 잠금)", name, n, m.label("Ctrl+L"))
	return m.lockTicker()
}

// lockTicker 는 열린 잠금이 있으면 lockTick 뒤 다시 그리게 한다. 한 번에 하나만 돈다.
func (m *model) lockTicker() tea.Cmd {
	if m.lockTicking || len(m.unlocked) == 0 {
		return nil
	}
	m.lockTicking = true
	return tea.Tick(lockTick, func(time.Time) tea.Msg { return lockTickMsg{} })
}

// expireLocks 는 시간이 다 된 잠금을 닫고, 닫은 접속 이름을 알린다.
func (m *model) expireLocks() {
	for c, until := range m.unlocked {
		if !lockNow().Before(until) {
			delete(m.unlocked, c)
			m.status = fmt.Sprintf("prod(%s) 쓰기 다시 잠김", m.conn(c).Name)
		}
	}
}

// lockBadge 는 헤더에 보일 남은 시간(`🔓 prod 9:41`)이다. 활성 탭 접속이 열려 있을 때만 보인다.
func (m *model) lockBadge(conn int) string {
	until, ok := m.unlocked[conn]
	if !ok || !m.prodLock {
		return ""
	}
	left := until.Sub(lockNow())
	if left <= 0 {
		return ""
	}
	s := int(left.Round(time.Second).Seconds())
	return fmt.Sprintf("🔓 prod %d:%02d", s/60, s%60)
}
