package main

import (
	"context"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/redis/go-redis/v9"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

// ─── R50 되돌리기 보강 ───

func TestUndo_RETURNING_권한이_없거나_바꾼_행이_너무_크면_쌓지_않고_이유를_알린다(t *testing.T) {
	m := testModel(t)
	tab := m.cur()
	tab.conn = 1
	cs, applied := submitted()
	applied.NoReturning = true
	if note, cmd := m.pushUndo(tab, 1, cs, applied); note != "되돌리기 불가: RETURNING 권한이 없어 바꾼 행을 받지 못함" || cmd != nil || len(tab.undo) != 0 {
		t.Fatalf("권한: %q undo=%d", note, len(tab.undo))
	}
	cs, applied = submitted()
	big := strings.Repeat("x", maxUndoBytes)
	applied.Rows[0]["name"] = &big
	if note, cmd := m.pushUndo(tab, 1, cs, applied); note != "되돌리기 불가: 바꾼 행이 너무 큼" || cmd != nil || len(tab.undo) != 0 {
		t.Fatalf("크기: %q undo=%d", note, len(tab.undo))
	}
}

func TestUndo_기록은_비밀_같은_열_값을_가리고_탭의_묶음은_그대로다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	m := testModel(t)
	tab := m.cur()
	tab.conn = 1
	one, newPw, oldPw, name := "1", "new-pw", "old-pw", "n"
	cs := db.ChangeSet{Table: db.TableRef{Schema: "public", Table: "t"}, PK: []string{"id"}, Changes: []db.Change{{Op: db.ChangeUpdate,
		Key: map[string]string{"id": "1"}, Values: map[string]*string{"password": &newPw, "name": &name}, Old: map[string]*string{"password": &oldPw, "name": &name}}}}
	applied := &db.Applied{Rows: []map[string]*string{{"id": &one, "password": &newPw, "name": &name}}}
	_, cmd := m.pushUndo(tab, 1, cs, applied)
	collect(cmd)
	path, _ := undoPath()
	got := readFile(t, path)
	if strings.Contains(got, "pw") || !strings.Contains(got, `\"password\" = '***'`) || !strings.Contains(got, `\"name\" = 'n'`) {
		t.Fatalf("기록: %s", got)
	}
	if v := tab.undo[0].cs.Changes[0].Values["password"]; v == nil || *v != oldPw {
		t.Fatalf("탭 묶음은 가리지 않는다: %v", v)
	}
}

func TestUndo_기록이_5MiB를_넘으면_점1로_옮기고_새로_쓴다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	path, _ := undoPath()
	must(t, os.MkdirAll(strings.TrimSuffix(path, "undo.jsonl"), 0o700))
	must(t, os.WriteFile(path, []byte(strings.Repeat("x", maxUndoLog-10)+"\n"), 0o600))
	must(t, os.WriteFile(path+".1", []byte("older\n"), 0o600))
	must(t, appendUndo(undoLine{Conn: "s", Undo: []string{"select 1;"}}))
	if st, err := os.Stat(path + ".1"); err != nil || st.Size() != maxUndoLog-9 {
		t.Fatalf(".1 은 넘친 파일이어야 한다: %v %v", st, err)
	}
	if got := readFile(t, path); strings.Count(got, "\n") != 1 || !strings.Contains(got, `"conn":"s"`) {
		t.Fatalf("새 파일: %q", got)
	}
	must(t, appendUndo(undoLine{Conn: "s2"}))
	if got := readFile(t, path); strings.Count(got, "\n") != 2 {
		t.Fatalf("작으면 덧붙인다: %q", got)
	}
}

func TestUndo_맨_위가_다른_데이터베이스_묶음이면_지금_DB의_가장_최근_묶음을_내고_없으면_버릴지_묻는다(t *testing.T) {
	m := testModel(t)
	tab := m.cur()
	tab.conn = 1
	m.proj.Conns[1].DB = "a"
	cs, applied := submitted()
	m.pushUndo(tab, 1, cs, applied)
	first := tab.undo[0]
	m.proj.Conns[1].DB = "b"
	cs, applied = submitted()
	m.pushUndo(tab, 1, cs, applied)
	m.proj.Conns[1].DB = "a"
	m.setFocus(fResult)
	m.Update(key("ctrl+z"))
	if m.overlay != oConfirm || m.pending == nil || m.pending.undo != first || m.status != "다른 데이터베이스에서 낸 묶음 1개 건너뜀" {
		t.Fatalf("지금 DB 묶음: overlay=%v status=%q", m.overlay, m.status)
	}
	m.Update(key("n"))

	m.proj.Conns[1].DB = "c"
	m.Update(key("ctrl+z"))
	if m.overlay != oConfirm || m.yes == nil || !strings.Contains(m.View(), "그 묶음 2개 버리기: y/N") {
		t.Fatalf("버릴지 묻는다:\n%s", m.View())
	}
	m.Update(key("n"))
	if len(tab.undo) != 2 {
		t.Fatal("N 이면 남긴다")
	}
	m.Update(key("ctrl+z"))
	m.Update(key("y"))
	if len(tab.undo) != 0 || m.status != "되돌리기 묶음을 버림" {
		t.Fatalf("y 면 버린다: undo=%d status=%q", len(tab.undo), m.status)
	}
}

// ─── R54 잠금 보강 ───

func TestProdLock_편집기의_COMMIT_ROLLBACK은_잠겨도_되고_BEGIN_SET은_F10F11을_안내하며_거절한다(t *testing.T) {
	m := testModel(t)
	m.prodLock = true
	tab := m.cur()
	tab.conn = 0
	for _, sql := range []string{"commit", "ROLLBACK", "end;", "abort", "-- 끝\nrollback to savepoint s", "commit and chain"} {
		m.status = ""
		m.run(sql)
		if strings.Contains(m.status, "쓰기 잠김") || m.overlay != oConfirm {
			t.Fatalf("%q 는 잠금이 막지 않고 R6 확인을 받는다: status=%q", sql, m.status)
		}
		m.Update(key("n"))
	}
	const locked = "prod(x-prod) 쓰기 잠김 — Ctrl+L 로 10분 열기"
	for sql, want := range map[string]string{
		"commit prepared 'x'":         locked,
		"commit; delete from t":       locked,
		"begin":                       locked + " · 트랜잭션 끝내기는 F10/F11",
		"set statement_timeout = 0":   locked + " · 트랜잭션 끝내기는 F10/F11",
		"update t set a = 1 where id": locked,
	} {
		m.status = ""
		m.run(sql)
		if m.status != want || m.overlay == oConfirm {
			t.Fatalf("%q: status=%q", sql, m.status)
		}
	}
	m.inTx[0] = true
	m.run("update t set a = 1 where id = 1")
	if m.status != locked+" · 트랜잭션 끝내기는 F10/F11" {
		t.Fatalf("열린 트랜잭션이 있으면 안내한다: %q", m.status)
	}
}

func TestProdLock_확인과_시작_사이에_잠금이_끝나면_y여도_쓰지_않는다(t *testing.T) {
	m := testModel(t)
	m.prodLock = true
	now := time.Date(2026, 11, 15, 9, 0, 0, 0, time.UTC)
	old := lockNow
	lockNow = func() time.Time { return now }
	t.Cleanup(func() { lockNow = old })
	tab := m.cur()
	tab.conn = 0
	m.Update(keyMsgOf("ctrl+l"))
	m.run("update t set a = 1 where id = 1")
	if m.overlay != oConfirm {
		t.Fatal("열린 동안은 R6 확인")
	}
	now = now.Add(11 * time.Minute)
	_, cmd := m.Update(key("y"))
	if cmd != nil || tab.running != nil || !strings.HasPrefix(m.status, "prod(x-prod) 쓰기 잠김") {
		t.Fatalf("시작할 때 다시 본다: status=%q", m.status)
	}
}

func TestProdLock_파라미터가_있는_쓰기는_값을_묻기_전에_거절한다(t *testing.T) {
	m := testModel(t)
	m.prodLock = true
	m.cur().conn = 0
	m.run("update t set a = :a where id = :id")
	if m.overlay == oParams || m.params != nil || !strings.HasPrefix(m.status, "prod(x-prod) 쓰기 잠김") {
		t.Fatalf("overlay=%v status=%q", m.overlay, m.status)
	}
	m.run("select :id")
	if m.overlay != oParams {
		t.Fatal("읽기는 값을 묻는다")
	}
}

func TestProdLock_여는_시간은_하루까지이고_잘못된_값은_기본값이다(t *testing.T) {
	for in, want := range map[string]int{"30": 30, "1440": 1440, "100000": 1440, "999999999999": 1440, "0": 10, "-5": 10, "abc": 10, "": 10} {
		t.Setenv("DB_PROD_UNLOCK_MIN", in)
		if got := unlockMinutes(); got != want {
			t.Errorf("%q: %d want %d", in, got, want)
		}
	}
}

func TestProdLock_실제_Redis에서_잠긴_prod의_쓰기_명령은_보내지_않는다(t *testing.T) {
	addr := os.Getenv("DBTUI_TEST_REDIS_ADDR")
	if addr == "" {
		t.Skip("DBTUI_TEST_REDIS_ADDR not set")
	}
	cl := redis.NewClient(&redis.Options{Addr: addr})
	defer cl.Close()
	ctx := context.Background()
	must(t, cl.Del(ctx, "v7fix:lock").Err())
	t.Cleanup(func() { cl.Del(ctx, "v7fix:lock") })
	m := testModel(t)
	m.prodLock = true
	host, port, _ := strings.Cut(addr, ":")
	p, err := strconv.Atoi(port)
	must(t, err)
	m.proj.Conns = append(m.proj.Conns, db.Conn{Name: "r-prod", UUID: "rp", Kind: db.KindRedis, Host: host, Port: p, DB: "0"})
	t.Cleanup(func() { m.stop(); m.clients.Close() })
	m.cur().conn = 2
	if cmd := m.run("SET v7fix:lock 1"); cmd != nil || m.status != "prod(r-prod) 쓰기 잠김 — Ctrl+L 로 10분 열기" {
		t.Fatalf("status=%q", m.status)
	}
	drive(m, m.run("GET v7fix:lock"))
	if n, _ := cl.Exists(ctx, "v7fix:lock").Result(); n != 0 || strings.Contains(m.status, "실패") {
		t.Fatalf("쓰기는 가지 않고 읽기는 된다: exists=%d status=%q", n, m.status)
	}
}

func TestProdLock_실제_서버에서_잠긴_prod의_읽기_판정_문장은_READ_ONLY로_돌아_숨은_쓰기가_거절된다(t *testing.T) {
	m := pgModel(t, "it-prod")
	m.prodLock = true
	it := pgTest{t, m}
	it.sql("drop table if exists public.v7fix_lock; create table public.v7fix_lock (id int primary key); insert into public.v7fix_lock values (1), (2)")
	t.Cleanup(func() { it.sql("drop table if exists public.v7fix_lock") })
	m.cur().conn = 0
	const hidden = "with d as (delete from public.v7fix_lock where id = 1 returning *) select * from d"
	drive(m, m.run(hidden))
	// 다른 실패처럼 상태 줄은 "실패", 서버 오류는 결과 영역에 보인다
	if m.status != "실패" || !strings.Contains(m.cur().resErr, "read-only transaction") || it.sql("select count(*) from public.v7fix_lock").Rows[0][0] != "2" {
		t.Fatalf("서버가 거절해야 한다: %q %q", m.status, m.cur().resErr)
	}
	drive(m, m.run("select count(*) from public.v7fix_lock"))
	if m.cur().res == nil || m.cur().res.Rows[0][0] != "2" || m.inTx[0] {
		t.Fatalf("읽기는 되고 트랜잭션을 남기지 않는다: %q", m.status)
	}
	// 열면 원래대로(감싸지 않고) 실행된다
	m.Update(keyMsgOf("ctrl+l"))
	drive(m, m.run(hidden))
	if got := it.sql("select count(*) from public.v7fix_lock").Rows[0][0]; got != "1" {
		t.Fatalf("열린 prod 는 그대로 실행: %s status=%q", got, m.status)
	}
}

// ─── R51·R53 조회 끝 ───

func TestLookup_여러_접속_조회와_스키마_비교가_끝나면_컨텍스트를_취소한다(t *testing.T) {
	m := testModel(t)
	tab, ctx := m.startMulti("x")
	m.showMulti(multiMsg{tab: tab.id, seq: tab.seq, titles: []string{"a"}, conns: []int{-1}, subs: []*db.Result{{Text: "ok"}}, errs: []error{nil}})
	if ctx.Err() == nil || tab.running != nil {
		t.Fatal("조회 컨텍스트를 놓아야 한다")
	}
	tab, ctx = m.startMulti("y")
	m.showSchemaDiff(schemaDiffMsg{tab: tab.id, seq: tab.seq, res: &db.Result{}})
	if ctx.Err() == nil || tab.running != nil {
		t.Fatal("스키마 비교 컨텍스트를 놓아야 한다")
	}
}

// ─── R57 세션 복원 ───

func TestSession_충돌_탭을_끼워도_고정한_탭은_앞에_모인다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	t.Setenv("DB_AUTO_SAVE", "0")
	m := testModel(t)
	file := filepath.Join(m.proj.Dir, "a.sql")
	must(t, os.WriteFile(file, []byte("select 1"), 0o644))
	a := m.cur()
	a.title, a.file, a.pinned = "a.sql", "a.sql", true
	a.editor.SetValue("select 1 -- 고침")
	b := m.newTab()
	b.title, b.pinned = "b", true
	must(t, m.saveSession())
	must(t, os.WriteFile(file, []byte("select 2"), 0o644))

	r := newModel(m.proj)
	r.restoreSession()
	if len(r.tabs) != 3 || !r.tabs[0].pinned || !r.tabs[1].pinned || r.tabs[2].pinned || r.tabs[2].conflictOf != "a.sql" {
		t.Fatalf("탭: %v", r.tabLabels())
	}
}
