package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"maps"
	"os"
	"path/filepath"
	"time"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/session"
	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

// ─── 제출 되돌리기 (R50) ───
// 제출이 성공하면 RETURNING * 로 받은 행으로 되돌리는 묶음을 만들어 탭에 쌓고 개인 상태 폴더 로그에 남긴다.
// Ctrl+Z 는 그 묶음을 보통 제출과 같은 길(guard → ApplyChanges)로 낸다. 옛값 확인이 실패하면 묶음은 남는다.

const (
	maxUndo = 20 // 탭마다 쌓는 되돌리기 묶음 수
	// 제출 하나에서 붙잡아 둘 RETURNING 행의 원값 바이트 합. 넘으면 되돌리지 않는다(탭마다 20개라 최악 160MiB)
	maxUndoBytes = 8 << 20
	// undo.jsonl 이 이 크기를 넘으면 undo.jsonl.1 로 옮기고 새로 쓴다. 옮긴 것은 하나만 둔다
	maxUndoLog = 5 << 20
	secretMask = "***"
)

// undoEntry 는 되돌리기 묶음 하나다. conn·db 는 제출한 접속과 그때의 데이터베이스다.
type undoEntry struct {
	cs   db.ChangeSet
	conn int
	db   string
}

// undoLine 은 undo.jsonl 한 줄이다. 되돌리기 SQL 에는 원래 값이 들어 개인 상태 폴더에만 둔다.
type undoLine struct {
	At    time.Time `json:"at"`
	Conn  string    `json:"conn"`
	Table string    `json:"table"`
	Undo  []string  `json:"undo"`
}

// undoPath 는 되돌리기 로그 자리다. 세션 파일(R41)과 같은 개인 상태 폴더에 둔다.
func undoPath() (string, error) {
	p, err := session.Path()
	if err != nil {
		return "", err
	}
	return filepath.Join(filepath.Dir(p), "undo.jsonl"), nil
}

// appendUndo 는 로그에 한 줄을 덧붙인다(0600). 명령 고루틴에서 부른다. 한 줄은 한 번의 write 라 다른 db 와 섞이지 않는다.
// 덧붙이면 maxUndoLog 를 넘을 때는 지금 파일을 .1 로 옮기고(이전 .1 은 덮는다) 새 파일에 쓴다.
// 한계: 두 db 가 동시에 옮기면 한쪽 줄이 .1 로 갈 수 있다. 되돌리기 로그는 덤이라 잠그지 않는다.
func appendUndo(line undoLine) error {
	path, err := undoPath()
	if err != nil {
		return err
	}
	data, err := json.Marshal(line)
	if err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return err
	}
	if st, err := os.Stat(path); err == nil && st.Size()+int64(len(data))+1 > maxUndoLog {
		if err := os.Rename(path, path+".1"); err != nil {
			return err
		}
	}
	f, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_APPEND, 0o600)
	if err != nil {
		return err
	}
	_, werr := f.Write(append(data, '\n'))
	return errors.Join(werr, f.Close())
}

// pushUndo 는 성공한 제출 cs 의 되돌리기 묶음을 탭에 쌓고 로그에 남기는 명령을 돌려준다.
// 묶음을 만들 수 없으면(잘린 칸이라 옛값을 모름 등) 그 이유를 상태 줄 글로 돌려준다.
// 기록에는 비밀 같은 이름(textutil.LooksSecret)의 열 값을 가려 남긴다. 탭에 쌓는 묶음은 가리지 않는다.
func (m *model) pushUndo(t *queryTab, conn int, cs db.ChangeSet, applied *db.Applied) (string, tea.Cmd) {
	if applied != nil && applied.NoReturning {
		return "되돌리기 불가: RETURNING 권한이 없어 바꾼 행을 받지 못함", nil
	}
	size := 0
	if applied != nil {
		for _, row := range applied.Rows {
			for col, v := range row {
				size += len(col)
				if v != nil {
					size += len(*v)
				}
			}
		}
	}
	if size > maxUndoBytes {
		return "되돌리기 불가: 바꾼 행이 너무 큼", nil
	}
	undo, err := db.UndoChanges(cs, applied)
	if err != nil {
		return "되돌리기 묶음을 만들지 못함: " + err.Error(), nil
	}
	t.undo = append(t.undo, &undoEntry{cs: undo, conn: conn, db: m.conn(conn).DB})
	if len(t.undo) > maxUndo {
		t.undo = t.undo[len(t.undo)-maxUndo:]
	}
	logged := undo
	logged.Changes = make([]db.Change, len(undo.Changes))
	mask := secretMask
	for i, ch := range undo.Changes {
		ch.Key, ch.Values, ch.Old = maskSecrets(ch.Key, mask), maskSecrets(ch.Values, &mask), maskSecrets(ch.Old, &mask)
		logged.Changes[i] = ch
	}
	line := undoLine{At: time.Now(), Conn: m.conn(conn).UUID, Table: cs.Table.Quoted(), Undo: db.ChangesSQL(logged)}
	return "", func() tea.Msg {
		if err := appendUndo(line); err != nil {
			return statusMsg("되돌리기 기록 실패: " + err.Error())
		}
		return nil
	}
}

// maskSecrets 는 비밀 같은 이름의 값을 mask 로 바꾼 사본이다. 그런 이름이 없으면 m 그대로다.
func maskSecrets[V any](m map[string]V, mask V) map[string]V {
	var out map[string]V
	for k := range m {
		if textutil.LooksSecret(k) {
			if out == nil {
				out = maps.Clone(m)
			}
			out[k] = mask
		}
	}
	if out == nil {
		return m
	}
	return out
}

// undoLast 는 탭의 마지막 되돌리기 묶음을 미리 보이고 확인 뒤 제출한다(Ctrl+Z).
// 위쪽 묶음이 제출한 뒤 데이터베이스를 바꾼 접속의 것이면 건너뛰고 지금 데이터베이스의 가장 최근 묶음을 낸다.
// 건너뛴 묶음은 그 데이터베이스로 돌아가면 다시 쓸 수 있어 지우지 않는다. 쓸 묶음이 하나도 없을 때만 버릴지 묻는다.
func (m *model) undoLast(t *queryTab) tea.Cmd {
	if len(t.undo) == 0 {
		m.status = "되돌릴 제출이 없음"
		return nil
	}
	i := len(t.undo) - 1
	for i >= 0 && m.conn(t.undo[i].conn).DB != t.undo[i].db {
		i--
	}
	if i < 0 {
		m.confirm(fmt.Sprintf("제출한 뒤 데이터베이스를 바꿔 되돌릴 묶음이 없음 — 그 묶음 %d개 버리기", len(t.undo)), func() tea.Cmd {
			t.undo = nil
			m.status = "되돌리기 묶음을 버림"
			return nil
		})
		return nil
	}
	e := t.undo[i]
	note := ""
	if skipped := len(t.undo) - 1 - i; skipped > 0 {
		note = fmt.Sprintf("다른 데이터베이스에서 낸 묶음 %d개 건너뜀", skipped)
		m.status = note // 확인 창에서도 보이게 한다. 시작하면 note 가 상태 줄 앞에 다시 붙는다
	}
	lines := db.ChangesSQL(e.cs)
	cs, c, clients := e.cs, m.conn(e.conn), m.clients
	return m.guard(pendingRun{tab: t.id, conn: e.conn, kind: rkSubmit, tx: true, stmts: len(lines), undo: e, note: note,
		confirm: fmt.Sprintf("제출 되돌리기 %d건", len(cs.Changes)), preview: lines,
		sql: fmt.Sprintf("-- 되돌리기 %d건 %s", len(cs.Changes), cs.Table.Quoted()),
		exec: func(ctx context.Context) (*db.Result, error) {
			pg, err := clients.PG(ctx, c)
			if err != nil {
				return nil, err
			}
			return pg.ApplyChanges(ctx, cs)
		}})
}
