package main

import (
	"bytes"
	"context"
	"crypto/aes"
	"crypto/cipher"
	"encoding/hex"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/redis/go-redis/v9"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/grid"
	"github.com/Jeong-Jae-Hun/i-db/internal/session"
	"github.com/Jeong-Jae-Hun/i-db/internal/tableview"
)

// TestMain 은 종료 때 세션을 저장하는(R41) 테스트들이 사용자의 실제 상태 폴더에 쓰지 않게 하고,
// 복사 테스트가 실제 시스템 클립보드 명령을 부르지 않게 OSC 52 만 쓰게 한다(R71.4).
func TestMain(m *testing.M) {
	dir, err := os.MkdirTemp("", "dbtui-state-")
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	os.Setenv("XDG_STATE_HOME", dir)
	os.Setenv("DB_CLIPBOARD", "osc52")
	code := m.Run()
	os.RemoveAll(dir)
	os.Exit(code)
}

// channels 는 orders 테이블 보기처럼 Origin 이 있는 결과다. user_id 는 1315, owner_id 는 NULL 이다.
func channels() *db.Result {
	return &db.Result{Cols: []string{"id", "user_id", "owner_id"}, Rows: [][]string{{"7", "1315", "NULL"}},
		Origin: &db.Origin{Table: db.TableRef{Schema: "public", Table: "orders"}, PK: []string{"id"}, Cols: []string{"id", "user_id", "owner_id"}}}
}

// ─── R36 외래 키 따라가기 ───

func TestFollow_F는_외래_키를_읽어_참조_테이블을_새_탭에_WHERE로_연다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	show(m, channels())
	m.setFocus(fResult)
	m.Update(key("l"))
	_, cmd := m.Update(key("F"))
	if cmd == nil || m.status != "외래 키 찾는 중…" {
		t.Fatalf("FK 를 읽는 명령: %q", m.status)
	}
	f := grid.Follow{Col: "user_id", Row: map[string]string{"id": "7", "user_id": "1315", "owner_id": "NULL"}}
	fks := []db.FK{{Name: "a_fk", Table: db.TableRef{Schema: "public", Table: "orders"}, Cols: []string{"user_id"},
		Ref: db.TableRef{Schema: "public", Table: "customers"}, RefCols: []string{"id"}}}
	m.Update(fkMsg{conn: 1, db: m.conn(1).DB, follow: f, fks: fks})
	tab := m.cur()
	if len(m.tabs) != 2 || tab.tv == nil || tab.title != "customers" || tab.conn != 1 || tab.running == nil {
		t.Fatalf("새 탭 테이블 보기: tabs=%d tv=%v title=%q", len(m.tabs), tab.tv != nil, tab.title)
	}
	if sql := tab.tv.SQL(); !strings.Contains(sql, `"public"."customers" WHERE "id" = '1315'`) {
		t.Fatalf("WHERE 를 채운다: %q", sql)
	}
}

func TestFollow_F는_FK가_아니거나_NULL이면_알리고_열지_않는다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	row := map[string]string{"id": "7", "user_id": "1315", "owner_id": "NULL"}
	fks := []db.FK{{Name: "b_fk", Cols: []string{"user_id", "owner_id"}, Ref: db.TableRef{Schema: "public", Table: "t_pairs"}, RefCols: []string{"u", "o"}}}
	m.Update(fkMsg{conn: 1, db: m.conn(1).DB, follow: grid.Follow{Col: "id", Row: row}, fks: fks})
	if m.status != "외래 키 열이 아님" || len(m.tabs) != 1 {
		t.Fatalf("FK 가 아님: %q", m.status)
	}
	// 여러 열 FK 는 모든 열 값을 쓰고 하나라도 NULL 이면 열지 않는다
	m.Update(fkMsg{conn: 1, db: m.conn(1).DB, follow: grid.Follow{Col: "user_id", Row: row}, fks: fks})
	if m.status != "참조 값이 NULL" || len(m.tabs) != 1 {
		t.Fatalf("NULL: %q", m.status)
	}
	m.Update(fkMsg{conn: 1, db: m.conn(1).DB, follow: grid.Follow{Col: "user_id", Row: row}, err: fmt.Errorf("boom")})
	if !strings.HasPrefix(m.status, "외래 키 읽기 실패") {
		t.Fatalf("오류: %q", m.status)
	}
	m.Update(fkMsg{conn: 1, db: "other", follow: grid.Follow{Col: "user_id", Row: row}, fks: fks})
	if !strings.Contains(m.status, "데이터베이스를 바꿔") || len(m.tabs) != 1 {
		t.Fatalf("DB 를 바꿨으면 열지 않는다: %q", m.status)
	}
}

func TestFollow_R은_참조하는_테이블을_골라_FK_열_WHERE로_연다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	row := map[string]string{"id": "1315", "name": "a"}
	fks := []db.FK{
		{Name: "c_fk", Table: db.TableRef{Schema: "public", Table: "orders"}, Cols: []string{"user_id"}, Ref: db.TableRef{Schema: "public", Table: "customers"}, RefCols: []string{"id"}},
		{Name: "o_fk", Table: db.TableRef{Schema: "shop", Table: "orders"}, Cols: []string{"buyer_id"}, Ref: db.TableRef{Schema: "public", Table: "customers"}, RefCols: []string{"id"}},
	}
	m.Update(fkMsg{conn: 1, db: m.conn(1).DB, follow: grid.Follow{Col: "id", Row: row, Reverse: true}, fks: fks})
	if m.overlay != oPicker || len(m.pickerItems) != 2 || m.pickerItems[1] != "shop.orders (buyer_id)" {
		t.Fatalf("목록: overlay=%v items=%v", m.overlay, m.pickerItems)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyDown})
	m.Update(key("enter"))
	tab := m.cur()
	if len(m.tabs) != 2 || tab.tv == nil || !strings.Contains(tab.tv.SQL(), `"shop"."orders" WHERE "buyer_id" = '1315'`) {
		t.Fatalf("고른 테이블을 연다: tabs=%d status=%q", len(m.tabs), m.status)
	}

	m.Update(fkMsg{conn: 1, db: m.conn(1).DB, follow: grid.Follow{Col: "id", Row: row, Reverse: true}})
	if m.status != "이 테이블을 참조하는 테이블이 없음" {
		t.Fatalf("없음: %q", m.status)
	}
}

func TestFollow_다른_접속의_결과에서는_외래_키를_따라가지_않는다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	show(m, channels())
	m.cur().conn = 0
	m.setFocus(fResult)
	m.Update(key("l"))
	if _, cmd := m.Update(key("F")); cmd != nil || !strings.Contains(m.status, "접속의 것이라 막음") {
		t.Fatalf("다른 접속 결과: %q", m.status)
	}
}

// ─── R37 복호화 보기 ───

// encrypt 는 staticEncrypt 와 같은 꼴 `<ivhex>^|^<datahex>` 를 만든다(AES-CBC, 키는 텍스트 바이트, PKCS#7).
func encrypt(t *testing.T, plain, key string) string {
	t.Helper()
	block, err := aes.NewCipher([]byte(key))
	must(t, err)
	iv := bytes.Repeat([]byte{7}, aes.BlockSize)
	pad := aes.BlockSize - len(plain)%aes.BlockSize
	data := append([]byte(plain), bytes.Repeat([]byte{byte(pad)}, pad)...)
	out := make([]byte, len(data))
	cipher.NewCBCEncrypter(block, iv).CryptBlocks(out, data)
	return hex.EncodeToString(iv) + "^|^" + hex.EncodeToString(out)
}

func TestDecrypt_D는_그룹_키로_풀어_보이고_다시_D면_원래_값이다(t *testing.T) {
	const aesKey = "0123456789abcdef"
	m := testModel(t)
	m.proj.Conns[1].Group = "app"
	m.cur().conn = 1
	enc := encrypt(t, "홍길동", aesKey)
	show(m, &db.Result{Cols: []string{"name"}, Rows: [][]string{{enc}}})
	m.setFocus(fResult)

	m.Update(key("D"))
	if m.status != "APP_AES_KEY 가 .env 에 없음" || gridCell(t, m) != enc {
		t.Fatalf("키가 없으면 풀지 않는다: %q", m.status)
	}

	m.proj.AESEnv = map[string]string{"APP_AES_KEY": aesKey}
	m.Update(key("D"))
	if m.status != "복호화 보기 (D 로 원래 값)" || gridCell(t, m) != "홍길동" || strings.Contains(m.status, aesKey) {
		t.Fatalf("푼 값을 보인다: %q cell=%q", m.status, gridCell(t, m))
	}
	m.Update(key("D"))
	if gridCell(t, m) != enc || m.cur().grid.Decrypted() {
		t.Fatal("다시 D 면 원래 값")
	}
}

func TestDecrypt_풀지_못한_칸은_원래_값으로_두고_수를_알린다(t *testing.T) {
	const aesKey = "0123456789abcdef"
	m := testModel(t)
	m.proj.Conns[1].Group = "auth"
	m.proj.AESEnv = map[string]string{"AUTH_AES_KEY": aesKey}
	m.cur().conn = 1
	bad := encrypt(t, "x", "fedcba9876543210")
	show(m, &db.Result{Cols: []string{"a", "b"}, Rows: [][]string{{encrypt(t, "ok", aesKey), bad}}})
	m.setFocus(fResult)
	m.Update(key("D"))
	if m.status != "1칸 풀지 못함" || strings.Contains(m.View(), aesKey) {
		t.Fatalf("못 푼 칸 수: %q", m.status)
	}
	m.Update(key("l"))
	if gridCell(t, m) != bad {
		t.Fatal("못 푼 칸은 원래 값")
	}
}

// ─── R38 칸 값으로 거르기 ───

// tableResult 는 탭에 x-stg(접속 1)의 public.t 테이블 보기와 그 페이지 결과를 붙인다.
func tableResult(m *model) *queryTab {
	tab := m.cur()
	tab.conn = 1
	tv := tableview.New(db.TableRef{Schema: "public", Table: "t"}, tableview.Options{})
	tab.tv = &tv
	res := &db.Result{Cols: []string{"id", "status"}, Rows: [][]string{{"1", "ACTIVE"}, {"2", "NULL"}},
		Origin: &db.Origin{Table: db.TableRef{Schema: "public", Table: "t"}, PK: []string{"id"}, Cols: []string{"id", "status"}}}
	m.Update(resultMsg{tab: tab.id, seq: tab.seq, conn: 1, kind: rkTable, res: res, rev: tab.grid.Revision()})
	return tab
}

func TestCellFilter_테이블_보기에서_더하기_빼기는_WHERE를_토글하고_다시_읽는다(t *testing.T) {
	m := testModel(t)
	tab := tableResult(m)
	m.setFocus(fResult)
	m.Update(key("l"))
	if _, cmd := m.Update(key("+")); cmd == nil || tab.running == nil || !strings.Contains(tab.tv.SQL(), `WHERE "status" = 'ACTIVE'`) {
		t.Fatalf("+ 는 = 조건을 더하고 다시 읽는다: %q", tab.tv.SQL())
	}
	m.Update(key("+"))
	if strings.Contains(tab.tv.SQL(), "WHERE") {
		t.Fatalf("같은 조건이면 뺀다: %q", tab.tv.SQL())
	}
	m.Update(key("j"))
	m.Update(key("-"))
	if !strings.Contains(tab.tv.SQL(), `WHERE "status" IS NOT NULL`) {
		t.Fatalf("NULL 칸의 - 는 IS NOT NULL: %q", tab.tv.SQL())
	}
}

func TestCellFilter_테이블_보기가_아니면_거절한다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	show(m, editable())
	m.setFocus(fResult)
	if _, cmd := m.Update(key("+")); cmd != nil || m.status != "테이블 보기에서만 칸으로 거르기" || m.cur().running != nil {
		t.Fatalf("테이블 보기가 아님: %q", m.status)
	}
}

// ─── R39 쓰기 전 영향 행 수 ───

func TestEstimate_prod_쓰기_확인에_추정_행_수를_더하고_늦은_추정은_버린다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 0
	cmd := m.run("delete from t_logs where created_at < '2024-01-01'")
	if cmd == nil || m.overlay != oConfirm || !strings.Contains(m.View(), "prod(x-prod)에 쓰기 실행: y/N") {
		t.Fatalf("확인 창은 추정을 기다리지 않고 바로 뜬다:\n%s", m.View())
	}
	// 접속할 수 없으면 추정은 실패로 돌아오고 창에는 아무것도 더하지 않는다
	if msg, ok := cmd().(estimateMsg); !ok || msg.err == nil {
		t.Fatalf("추정 명령은 estimateMsg 를 돌려준다: %#v", msg)
	}
	old := m.pending.ask
	m.Update(estimateMsg{id: old, rows: 120000})
	if !strings.Contains(m.View(), "prod(x-prod)에 쓰기 실행 · 약 120,000행 (추정): y/N") {
		t.Fatalf("추정을 더한다:\n%s", m.View())
	}
	m.Update(key("n"))

	m.run("update t set a = 1 where id = 2")
	m.Update(estimateMsg{id: old, rows: 5})
	m.Update(estimateMsg{id: m.pending.ask, err: context.DeadlineExceeded})
	if strings.Contains(m.View(), "추정") {
		t.Fatalf("이전 확인의 추정과 실패한 추정은 보이지 않는다:\n%s", m.View())
	}
	if _, cmd := m.Update(key("y")); cmd == nil || m.cur().running == nil {
		t.Fatal("추정 없이도 y 면 실행한다")
	}
}

func TestEstimate_위험_확인에도_보이고_INSERT와_여러_문장은_추정하지_않는다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	m.run("delete from t")
	m.Update(estimateMsg{id: m.pending.ask, rows: 3})
	if !strings.Contains(m.View(), "위험: WHERE 없는 DELETE · 약 3행 (추정) — 실행할까요? y/N") {
		t.Fatalf("위험 확인:\n%s", m.View())
	}
	m.Update(key("n"))
	m.cur().conn = 0
	if cmd := m.run("insert into t values (1)"); cmd != nil || m.overlay != oConfirm {
		t.Fatal("INSERT 는 추정하지 않는다")
	}
	m.Update(key("n"))
	if cmd := m.run("delete from a where id = 1; delete from b where id = 1"); cmd != nil || m.overlay != oConfirm {
		t.Fatal("여러 문장은 추정하지 않는다")
	}
}

// ─── R40 명령 팔레트 ───

func TestPalette_CtrlP로_열고_고르면_그_포커스에서_키를_누른다(t *testing.T) {
	m := testModel(t)
	m.setFocus(fEditor)
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlP})
	if m.overlay != oPicker || len(m.pickerItems) < 50 {
		t.Fatalf("도움말 항목으로 연다: overlay=%v n=%d", m.overlay, len(m.pickerItems))
	}
	typeText(m, "ctrl+t")
	m.Update(key("enter"))
	if len(m.tabs) != 2 || m.overlay != oNone {
		t.Fatalf("Ctrl+T 를 누른 것과 같다: tabs=%d", len(m.tabs))
	}

	m.cur().conn = 1
	show(m, editable())
	m.setFocus(fEditor)
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlP})
	typeText(m, "칸 값 =")
	m.Update(key("enter"))
	if m.focus != fResult || m.status != "테이블 보기에서만 칸으로 거르기" {
		t.Fatalf("결과 항목은 결과 포커스로 옮겨 누르고, 쓸 수 없으면 그 안내를 보인다: focus=%v status=%q", m.focus, m.status)
	}
}

// palette 는 Ctrl+P 팔레트에서 label 로 걸러 Enter 를 누른다.
func palette(m *model, label string) tea.Cmd {
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlP})
	typeText(m, label)
	_, cmd := m.Update(key("enter"))
	return cmd
}

func TestPalette_위험한_키를_다시_넣어도_확인을_거친다(t *testing.T) {
	// 결과 K: pid 행 백엔드 신호는 늘 묻는다
	m := testModel(t)
	m.cur().conn = 1
	show(m, &db.Result{Cols: []string{"pid"}, Types: []string{"int4"}, Rows: [][]string{{"123"}}})
	m.setFocus(fEditor)
	palette(m, "pid 행의 쿼리 취소")
	if m.overlay != oConfirm || m.pending == nil || m.pending.confirm != "pid 123 쿼리 취소" || m.cur().running != nil {
		t.Fatalf("K: overlay=%v pending=%+v status=%q", m.overlay, m.pending, m.status)
	}
	m.Update(key("n"))

	// 결과 Ctrl+S: 보류 중인 편집 제출은 늘 묻는다
	show(m, editable())
	m.setFocus(fResult)
	m.Update(key("d"))
	m.setFocus(fEditor)
	palette(m, "변경 미리 보기 뒤 제출")
	if m.overlay != oConfirm || m.pending == nil || !strings.Contains(m.pending.confirm, "1건 제출") || m.cur().running != nil {
		t.Fatalf("Ctrl+S: overlay=%v status=%q", m.overlay, m.status)
	}
	m.Update(key("n"))

	// F10: prod COMMIT 은 묻는다
	m.cur().conn = 0
	m.inTx[0] = true
	palette(m, "COMMIT (prod 는 확인)")
	if m.overlay != oConfirm || m.pending == nil || m.pending.kind != rkTx || m.cur().running != nil {
		t.Fatalf("F10: overlay=%v status=%q", m.overlay, m.status)
	}
	m.Update(key("n"))

	// 트리 X: 패턴을 묻고, 센 뒤에도 지우기 전에 묻는다
	rn := addRedis(m)
	m.selectNode(rn)
	palette(m, "패턴 맞는 키 일괄 삭제")
	if m.overlay != oPrompt || m.focus != fTree {
		t.Fatalf("X: overlay=%v focus=%v", m.overlay, m.focus)
	}
	typeText(m, "vg:*")
	m.Update(key("enter"))
	m.Update(keyScanMsg{conn: rn.conn, pattern: "vg:*", keys: redisKeyNames(2)})
	if m.overlay != oConfirm || m.unlink != nil {
		t.Fatalf("X 확인: overlay=%v", m.overlay)
	}
}

func TestEstimate_추정이_오기_전에_y를_누르면_한_번만_실행하고_늦은_추정은_버린다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 0
	m.run("delete from t where id = 1")
	ask := m.pending.ask
	if _, cmd := m.Update(key("y")); cmd == nil || m.cur().running == nil {
		t.Fatal("y 면 추정 없이 실행한다")
	}
	seq := m.cur().seq
	m.Update(estimateMsg{id: ask, rows: 7})
	if m.overlay != oNone || m.pending != nil || m.cur().seq != seq || strings.Contains(m.View(), "추정") {
		t.Fatalf("늦은 추정은 확인 창을 되살리거나 다시 실행하지 않는다: overlay=%v seq=%d/%d", m.overlay, m.cur().seq, seq)
	}
	if _, cmd := m.Update(key("y")); m.cur().seq != seq {
		t.Fatalf("두 번째 y 는 실행하지 않는다: %v", cmd)
	}
}

func TestPalette_도움말_키_이름은_같은_키_입력으로_되돌린다(t *testing.T) {
	for _, sec := range helpSections {
		for _, kv := range sec.keys {
			if kv[2] != "" && keyMsgOf(kv[2]).String() != kv[2] {
				t.Errorf("%s: %q → %q", kv[0], kv[2], keyMsgOf(kv[2]).String())
			}
		}
	}
}

// ─── R41 세션 복원 ───

func TestSession_끝낼_때_탭을_저장하고_다음에_되살린다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	m := testModel(t)
	must(t, os.MkdirAll(filepath.Join(m.proj.Dir, "queries"), 0o755))
	must(t, os.WriteFile(filepath.Join(m.proj.Dir, "queries", "a.sql"), []byte("select 2"), 0o644))
	m.cur().conn = 1
	m.cur().editor.SetValue("select 1")
	m.cur().editor.SetCursorOffset(3)
	t2 := m.newTab()
	t2.title, t2.file, t2.conn = "파일", "queries/a.sql", 0
	m.stamp(t2, "select 2") // 파일에서 읽은 탭이다
	t2.editor.SetValue("select 2 -- 고침")
	m.switchTab(0)
	if _, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlC}); !isQuit(cmd) {
		t.Fatal("Ctrl+C 로 끝낸다")
	}

	r := newModel(m.proj)
	r.restoreSession()
	if len(r.tabs) != 2 || r.tab != 0 || !strings.Contains(r.status, "탭 2개 되살림") {
		t.Fatalf("탭 수·활성 탭: %d %d %q", len(r.tabs), r.tab, r.status)
	}
	a, b := r.tabs[0], r.tabs[1]
	if a.editor.Value() != "select 1" || a.editor.CursorOffset() != 3 || a.conn != 1 || a.res != nil {
		t.Fatalf("첫 탭: %q %d %d", a.editor.Value(), a.editor.CursorOffset(), a.conn)
	}
	if b.title != "파일" || b.file != "queries/a.sql" || b.editor.Value() != "select 2 -- 고침" || b.conn != 0 {
		t.Fatalf("파일 탭: %q %q %q %d", b.title, b.file, b.editor.Value(), b.conn)
	}
}

// savedTabs 는 세션 파일에 저장된 이 프로젝트의 탭이다.
func savedTabs(t *testing.T, m *model) []session.Tab {
	t.Helper()
	path, err := session.Path()
	must(t, err)
	st, ok, err := session.Load(path, m.proj.Dir)
	if err != nil || !ok {
		t.Fatalf("세션 읽기: %v %v", ok, err)
	}
	return st.Tabs
}

func TestSession_파일_탭은_저장_안_한_고침이_있을_때만_글을_두고_없으면_파일을_다시_읽는다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	m := testModel(t)
	file := filepath.Join(m.proj.Dir, "queries", "a.sql")
	must(t, os.MkdirAll(filepath.Dir(file), 0o755))
	must(t, os.WriteFile(file, []byte("select 1"), 0o644))
	t1 := m.cur()
	t1.title, t1.file = "a.sql", "queries/a.sql"
	t1.editor.SetValue("select 1")
	must(t, m.saveSession())
	if tabs := savedTabs(t, m); tabs[0].Text != "" || tabs[0].Disk != "" {
		t.Fatalf("고침 없는 파일 탭은 글을 두지 않는다: %+v", tabs[0])
	}
	// 끝낸 뒤 다른 편집기에서 파일을 고쳤으면 고친 내용을 연다
	must(t, os.WriteFile(file, []byte("select 3"), 0o644))
	r := newModel(m.proj)
	r.restoreSession()
	if len(r.tabs) != 1 || r.cur().file != "queries/a.sql" || r.cur().editor.Value() != "select 3" {
		t.Fatalf("디스크 내용을 연다: %d %q", len(r.tabs), r.cur().editor.Value())
	}
}

func TestSession_저장_안_한_글이_있는데_파일이_바뀌었으면_파일을_열고_글은_새_탭에_둔다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	t.Setenv("DB_AUTO_SAVE", "0") // 자동 저장이 켜져 있으면 끝낼 때 파일에 쓰므로(R45.2) 저장 안 한 글이 세션에만 남는 경우를 만든다
	m := testModel(t)
	file := filepath.Join(m.proj.Dir, "queries", "a.sql")
	must(t, os.MkdirAll(filepath.Dir(file), 0o755))
	must(t, os.WriteFile(file, []byte("select 1"), 0o644))
	m.cur().title, m.cur().conn = "Query 1", 1
	t2 := m.newTab()
	t2.title, t2.file, t2.conn = "a.sql", "queries/a.sql", 1
	t2.editor.SetValue("select 1 -- 고침")
	m.switchTab(1)
	must(t, m.saveSession())
	if tabs := savedTabs(t, m); tabs[1].Text != "select 1 -- 고침" || tabs[1].Disk == "" {
		t.Fatalf("고친 글과 디스크 해시를 둔다: %+v", tabs[1])
	}
	must(t, os.WriteFile(file, []byte("select 2 -- git pull"), 0o644))

	r := newModel(m.proj)
	r.restoreSession()
	if len(r.tabs) != 3 || r.tab != 1 {
		t.Fatalf("탭 수·활성 탭: %d %d", len(r.tabs), r.tab)
	}
	f, u := r.tabs[1], r.tabs[2]
	if f.file != "queries/a.sql" || f.editor.Value() != "select 2 -- git pull" {
		t.Fatalf("파일 탭은 디스크 내용: %q %q", f.file, f.editor.Value())
	}
	if u.title != "a.sql (저장 안 한 글)" || u.file != "" || u.editor.Value() != "select 1 -- 고침" || u.conn != 1 {
		t.Fatalf("저장 안 한 글은 새 탭: %q %q %q %d", u.title, u.file, u.editor.Value(), u.conn)
	}
	if !strings.Contains(r.status, "파일이 바뀌어 저장 안 한 글을 새 탭에 둔 탭 1개") {
		t.Fatalf("알림: %q", r.status)
	}
}

func TestSession_프로젝트_밖_파일은_읽지_않고_상한에_걸려_비운_탭은_알린다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	m := testModel(t)
	outside := filepath.Join(filepath.Dir(m.proj.Dir), "outside.sql")
	must(t, os.WriteFile(outside, []byte("secret"), 0o644))
	path, err := session.Path()
	must(t, err)
	must(t, session.Save(path, m.proj.Dir, session.State{Tabs: []session.Tab{
		{Title: "a", File: "../outside.sql"},
		{Title: "b", Text: strings.Repeat("x", 1<<20+1)},
	}}))
	m.restoreSession()
	if a := m.tabs[0]; a.file != "" || a.editor.Value() != "" {
		t.Fatalf("프로젝트 밖 파일: %q %q", a.file, a.editor.Value())
	}
	if !strings.Contains(m.status, "글이 커서 비운 탭 1개") || !strings.Contains(m.status, "없어진 파일 1개") {
		t.Fatalf("알림: %q", m.status)
	}
}

func TestSession_없어진_파일과_모르는_접속은_끊고_깨진_파일은_알린다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	m := testModel(t)
	path, err := session.Path()
	must(t, err)
	must(t, session.Save(path, m.proj.Dir, session.State{Active: 1, Tabs: []session.Tab{
		{Title: "a", Text: "select 1", File: "queries/gone.sql", Conn: "s"},
		{Title: "b", Text: "select 2", Conn: "nope"},
	}}))
	m.restoreSession()
	a, b := m.tabs[0], m.tabs[1]
	if a.file != "" || a.editor.Value() != "select 1" || a.conn != 1 || b.conn != -1 || m.tab != 1 {
		t.Fatalf("파일·접속: file=%q conn=%d/%d tab=%d", a.file, a.conn, b.conn, m.tab)
	}
	if !strings.Contains(m.status, "없어진 파일 1개") || !strings.Contains(m.status, "모르는 접속 1개") {
		t.Fatalf("알림: %q", m.status)
	}

	must(t, os.WriteFile(path, []byte("{broken"), 0o600))
	r := newModel(m.proj)
	r.restoreSession()
	if len(r.tabs) != 1 || !strings.Contains(r.status, "세션을 되살리지 못함: 파일이 깨져 session.json.bad 로 옮기고 새로 시작") {
		t.Fatalf("깨진 파일: %d %q", len(r.tabs), r.status)
	}
	if _, err := os.Stat(session.BadPath(path)); err != nil {
		t.Fatalf("깨진 파일을 옮긴다: %v", err)
	}
}

func TestSession_DB_RESTORE가_0이면_탭은_저장도_복원도_하지_않는다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	t.Setenv("DB_RESTORE", "0")
	m := testModel(t)
	m.cur().editor.SetValue("select 1")
	m.setFocus(fTree)
	if _, cmd := m.Update(key("q")); !isQuit(cmd) {
		t.Fatal("q 로 끝낸다")
	}
	if tabs := savedTabs(t, m); len(tabs) != 0 {
		t.Fatalf("탭은 저장하지 않는다(창 크기만 남긴다): %+v", tabs)
	}
	path, err := session.Path()
	must(t, err)
	must(t, session.Save(path, m.proj.Dir, session.State{Tabs: []session.Tab{{Title: "a"}, {Title: "b"}}}))
	r := newModel(m.proj)
	r.restoreSession()
	if len(r.tabs) != 1 {
		t.Fatalf("되살리지 않는다: %d", len(r.tabs))
	}
	// 끝낼 때 읽어 둔 탭을 지우지 않는다(DB_RESTORE 를 다시 켜면 되살아난다)
	must(t, r.saveSession())
	if tabs := savedTabs(t, r); len(tabs) != 2 {
		t.Fatalf("읽어 둔 탭은 그대로 둔다: %+v", tabs)
	}
}

// ─── R42 Redis 패턴 일괄 삭제 ───

// redisKeyNames 는 vg:1:h … 꼴의 키 n개다.
func redisKeyNames(n int) []string {
	keys := make([]string, n)
	for i := range keys {
		keys[i] = fmt.Sprintf("vg:%d:h", i)
	}
	return keys
}

func TestRedisDelete_X는_패턴을_받아_미리_보이고_y면_지우고_esc로_멈춘다(t *testing.T) {
	m := testModel(t)
	rn := addRedis(m)
	m.selectNode(rn)
	m.setFocus(fTree)
	m.Update(key("X"))
	if m.overlay != oPrompt {
		t.Fatalf("패턴을 묻는다: %v", m.overlay)
	}
	typeText(m, "vg:*:h")
	if _, cmd := m.Update(key("enter")); cmd == nil || m.status != "지울 키 세는 중…" {
		t.Fatalf("SCAN 명령: %q", m.status)
	}

	keys := redisKeyNames(37)
	m.Update(keyScanMsg{conn: rn.conn, pattern: "vg:*:h", keys: keys})
	v := m.View()
	if m.overlay != oConfirm || !strings.Contains(v, "r-stg에서 키 37개 지움: y/N") || !strings.Contains(v, "vg:19:h") || strings.Contains(v, "vg:20:h") {
		t.Fatalf("개수와 앞 20개를 보이고 묻는다:\n%s", v)
	}
	m.Update(key("n"))
	if m.unlink != nil || m.status != "취소함" {
		t.Fatal("N 이면 지우지 않는다")
	}

	m.Update(keyScanMsg{conn: rn.conn, pattern: "vg:*:h", keys: keys})
	if _, cmd := m.Update(key("y")); cmd == nil || m.unlink == nil || m.unlink.total != 37 {
		t.Fatal("y 면 지우기 시작")
	}
	job := m.unlink
	m.Update(unlinkTickMsg{job: job})
	if m.status != "키 지우는 중… 0/37 (esc 멈춤)" {
		t.Fatalf("진행: %q", m.status)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	if !job.canceled || m.status != "키 지우기 멈춤 요청함" {
		t.Fatalf("esc 로 멈춘다: %q", m.status)
	}
	m.Update(unlinkDoneMsg{job: job, n: 10, err: context.Canceled})
	if m.unlink != nil || m.status != "키 지우기 멈춤: 10개 지움" {
		t.Fatalf("멈춘 곳까지 지운 수: %q", m.status)
	}

	m.Update(keyScanMsg{conn: rn.conn, pattern: "vg:*:h", keys: keys})
	m.Update(key("y"))
	_, cmd := m.Update(unlinkDoneMsg{job: m.unlink, n: 37})
	if m.status != "37개 지움" || cmd == nil || rn.loaded {
		t.Fatalf("끝나면 트리를 다시 읽는다: %q", m.status)
	}
}

func TestRedisDelete_10만개_넘으면_거절하고_별표는_FLUSHDB로_경고한다(t *testing.T) {
	m := testModel(t)
	rn := addRedis(m)
	m.Update(keyScanMsg{conn: rn.conn, pattern: "*", keys: redisKeyNames(100000), truncated: true})
	if m.overlay != oNone || m.status != "키가 100,000개를 넘어 지우지 않음 (패턴을 좁히세요)" {
		t.Fatalf("상한: %q", m.status)
	}
	m.Update(keyScanMsg{conn: rn.conn, pattern: "*", keys: redisKeyNames(3)})
	if !strings.Contains(m.View(), "r-stg에서 키 3개 지움 · 위험: FLUSHDB 와 같음: y/N") {
		t.Fatalf("* 경고:\n%s", m.View())
	}
	m.Update(key("n"))
	m.proj.Conns[rn.conn].Name = "r-prod"
	m.Update(keyScanMsg{conn: rn.conn, pattern: "a*", keys: redisKeyNames(3)})
	if !strings.Contains(m.View(), "prod(r-prod)에서 키 3개 지움: y/N") {
		t.Fatalf("prod 이름:\n%s", m.View())
	}
	m.Update(key("n"))
	m.Update(keyScanMsg{conn: rn.conn, pattern: "zz*"})
	if m.status != "맞는 키가 없음: zz*" || m.overlay != oNone {
		t.Fatalf("없음: %q", m.status)
	}
}

func TestRedisDelete_이름공간은_패턴을_채우고_PostgreSQL에서는_하지_않는다(t *testing.T) {
	m := testModel(t)
	rn := addRedis(m)
	folder := &node{kind: nFolder, label: "vg: (2)", key: "vg:", conn: rn.conn, parent: rn, loaded: true}
	rn.children = append(rn.children, folder)
	m.rebuild()
	m.selectNode(folder)
	m.setFocus(fTree)
	m.Update(key("X"))
	if m.overlay != oPrompt || m.input.Value() != "vg:*" {
		t.Fatalf("이름공간 패턴: %q", m.input.Value())
	}
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})

	// PostgreSQL 접속 줄의 X 는 접속 지우기라 DataGrip 접속이면 안내만 한다(R59.3)
	m.selectNode(m.connNode(0))
	if m.Update(key("X")); m.overlay != oNone || m.status != dataGripRefusal {
		t.Fatalf("PostgreSQL 접속: %q", m.status)
	}
}

func TestRedisDelete_이름공간의_glob_글자는_감싸고_별표_물음표만인_패턴은_FLUSHDB로_경고한다(t *testing.T) {
	m := testModel(t)
	rn := addRedis(m)
	folder := &node{kind: nFolder, label: `a*b?[c]\: (1)`, key: `a*b?[c]\:`, conn: rn.conn, parent: rn, loaded: true}
	rn.children = append(rn.children, folder)
	m.rebuild()
	m.selectNode(folder)
	m.setFocus(fTree)
	m.Update(key("X"))
	if want := `a\*b\?\[c\]\\:*`; m.input.Value() != want {
		t.Fatalf("이름공간 패턴: %q, want %q", m.input.Value(), want)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	for pattern, warn := range map[string]bool{"**": true, "?*": true, "*?*": true, "??": false, "a*": false} {
		m.Update(keyScanMsg{conn: rn.conn, pattern: pattern, keys: redisKeyNames(3)})
		if got := strings.Contains(m.View(), "위험: FLUSHDB 와 같음"); got != warn {
			t.Errorf("%s: 경고 %v, want %v", pattern, got, warn)
		}
		m.Update(key("n"))
	}
}

func TestRedisDelete_지우는_중에는_q_Ctrl_C_트랜잭션_확인으로도_끝내지_않는다(t *testing.T) {
	m := testModel(t)
	rn := addRedis(m)
	m.Update(keyScanMsg{conn: rn.conn, pattern: "vg:*:h", keys: redisKeyNames(3)})
	m.Update(key("y"))
	job := m.unlink
	m.setFocus(fTree)
	const refuse = "키를 지우는 중 — esc 로 멈춘 뒤 종료"
	if _, cmd := m.Update(key("q")); isQuit(cmd) || m.status != refuse {
		t.Fatalf("q: %q", m.status)
	}
	if _, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlC}); isQuit(cmd) || m.status != refuse {
		t.Fatalf("Ctrl+C: %q", m.status)
	}
	// 트랜잭션 확인 창에서 한 번 더 누르는 강제 종료도 막는다
	m.inTx[0] = true
	m.overlay = oTx
	m.txAsk = &txPrompt{tab: m.cur().id, conns: []int{0}, after: afterQuit}
	if _, cmd := m.Update(tea.KeyMsg{Type: tea.KeyCtrlC}); isQuit(cmd) || m.status != refuse {
		t.Fatalf("확인 창 Ctrl+C: %q", m.status)
	}
	m.overlay, m.txAsk, m.inTx[0] = oNone, nil, false
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	m.Update(unlinkDoneMsg{job: job, n: 1, err: context.Canceled})
	if _, cmd := m.Update(key("q")); !isQuit(cmd) {
		t.Fatalf("멈춘 뒤에는 끝낸다: %q", m.status)
	}
}

func TestAsync_다른_창이_열려_있으면_늦게_온_결과로_창을_바꾸지_않는다(t *testing.T) {
	m := testModel(t)
	rn := addRedis(m)
	m.cur().conn = 1
	called := false
	open := func() tea.Cmd { called = true; return nil }
	f := grid.Follow{Col: "user_id", Reverse: true, Row: map[string]string{"id": "7", "user_id": "1315", "owner_id": "NULL"}}
	fks := []db.FK{{Name: "a_fk", Table: db.TableRef{Schema: "public", Table: "t_x"}, Cols: []string{"c"}, RefCols: []string{"id"}}}
	msgs := map[string]tea.Msg{
		"keyScan":     keyScanMsg{conn: rn.conn, pattern: "vg:*", keys: redisKeyNames(3)},
		"fk":          fkMsg{conn: 1, db: m.conn(1).DB, follow: f, fks: fks},
		"pickList":    pickListMsg{conn: 1, prompt: "DB ", items: []string{"a", "b"}, do: func(string) tea.Cmd { return nil }},
		"searchCount": searchCountMsg{conn: 1, n: 3, open: open},
	}
	for name, msg := range msgs {
		m.prompt("다른 창 ", "그대로", func(string) tea.Cmd { return nil })
		m.Update(msg)
		if m.overlay != oPrompt || m.input.Value() != "그대로" || m.status != "다른 창이 열려 있어 결과를 버림" {
			t.Errorf("%s: overlay=%v input=%q status=%q", name, m.overlay, m.input.Value(), m.status)
		}
		m.Update(tea.KeyMsg{Type: tea.KeyEsc})
		m.status = ""
	}
	if called {
		t.Fatal("버린 결과의 동작을 부르지 않는다")
	}
}

// ─── 실제 서버 ───

func TestFollow_실제_서버에서_F와_R로_외래_키를_따라간다(t *testing.T) {
	m := pgModel(t, "it-local")
	it := pgTest{t, m}
	it.sql(`drop table if exists public.v5ui_channels; drop table if exists public.v5ui_users;
		create table public.v5ui_users (id int primary key, name text);
		create table public.v5ui_channels (id int primary key, user_id int references public.v5ui_users(id));
		insert into public.v5ui_users values (1315, 'a'), (1318, 'b');
		insert into public.v5ui_channels values (1, 1315), (2, 1318), (3, 1315)`)
	t.Cleanup(func() { it.sql("drop table if exists public.v5ui_channels; drop table if exists public.v5ui_users") })

	m.cur().conn = 0
	m.cur().editor.SetValue("select * from public.v5ui_channels order by id")
	drive(m, m.run(m.target(m.cur())))
	m.setFocus(fResult)
	m.Update(key("l"))
	_, cmd := m.Update(key("F"))
	drive(m, cmd)
	tab := m.cur()
	if len(m.tabs) != 2 || tab.res == nil || len(tab.res.Rows) != 1 || tab.res.Rows[0][0] != "1315" {
		t.Fatalf("참조 테이블을 WHERE 로 연다: tabs=%d status=%q", len(m.tabs), m.status)
	}

	m.setFocus(fResult)
	_, cmd = m.Update(key("R"))
	drive(m, cmd)
	if m.overlay != oPicker || len(m.pickerItems) != 1 || m.pickerItems[0] != "public.v5ui_channels (user_id)" {
		t.Fatalf("참조하는 테이블 목록: %v status=%q", m.pickerItems, m.status)
	}
	_, cmd = m.Update(key("enter"))
	drive(m, cmd)
	if tab = m.cur(); len(m.tabs) != 3 || tab.res == nil || len(tab.res.Rows) != 2 {
		t.Fatalf("이 행을 참조하는 행: tabs=%d status=%q", len(m.tabs), m.status)
	}
}

func TestEstimate_실제_서버에서_위험_확인에_추정_행_수를_보인다(t *testing.T) {
	m := pgModel(t, "it-local")
	it := pgTest{t, m}
	it.sql(`drop table if exists public.v5ui_est; create table public.v5ui_est (id int);
		insert into public.v5ui_est select generate_series(1, 500); analyze public.v5ui_est`)
	t.Cleanup(func() { it.sql("drop table if exists public.v5ui_est") })
	m.cur().conn = 0
	drive(m, m.run("delete from public.v5ui_est"))
	if m.overlay != oConfirm || !strings.Contains(m.View(), "위험: WHERE 없는 DELETE · 약 500행 (추정)") {
		t.Fatalf("추정:\n%s", m.View())
	}
	m.Update(key("n"))
	if res := it.sql("select count(*) from public.v5ui_est"); res.Rows[0][0] != "500" {
		t.Fatal("추정은 문장을 실행하지 않는다")
	}
}

func TestRedisDelete_실제_서버에서_패턴_키만_지우고_트리를_다시_읽는다(t *testing.T) {
	addr := os.Getenv("DBTUI_TEST_REDIS_ADDR")
	if addr == "" {
		t.Skip("DBTUI_TEST_REDIS_ADDR not set")
	}
	cl := redis.NewClient(&redis.Options{Addr: addr})
	defer cl.Close()
	ctx := context.Background()
	for i := range 1203 {
		must(t, cl.Set(ctx, fmt.Sprintf("v5ui:del:%d", i), "x", 0).Err())
	}
	must(t, cl.Set(ctx, "v5ui:keep", "x", 0).Err())
	t.Cleanup(func() { cl.Del(ctx, "v5ui:keep") })

	m := testModel(t)
	host, port, _ := strings.Cut(addr, ":")
	p, err := strconv.Atoi(port)
	must(t, err)
	m.proj.Conns = append(m.proj.Conns, db.Conn{Name: "r-local", UUID: "rl", Kind: db.KindRedis, Host: host, Port: p, DB: "0"})
	t.Cleanup(func() { m.stop(); m.clients.Close() })
	rn := &node{kind: nConn, label: "r-local", conn: 2, parent: m.roots[0]}
	m.roots[0].children = append(m.roots[0].children, rn)
	m.rebuild()
	m.selectNode(rn)
	m.setFocus(fTree)
	m.Update(key("X"))
	typeText(m, "v5ui:del:*")
	_, cmd := m.Update(key("enter"))
	drive(m, cmd)
	if !strings.Contains(m.View(), "r-local에서 키 1,203개 지움: y/N") {
		t.Fatalf("개수:\n%s", m.View())
	}
	_, cmd = m.Update(key("y"))
	drive(m, cmd)
	if m.status != "1,203개 지움" || !rn.loaded {
		t.Fatalf("지운 수와 다시 읽은 트리: %q", m.status)
	}
	if n, _ := cl.Exists(ctx, "v5ui:keep", "v5ui:del:0", "v5ui:del:1202").Result(); n != 1 {
		t.Fatalf("패턴 키만 지운다: 남은 %d개", n)
	}
}

func TestSession_파일_탭의_글을_모두_지웠으면_빈_글로_되살린다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	m := testModel(t)
	file := filepath.Join(m.proj.Dir, "queries", "a.sql")
	must(t, os.MkdirAll(filepath.Dir(file), 0o755))
	must(t, os.WriteFile(file, []byte("select 1"), 0o644))
	t1 := m.cur()
	t1.title, t1.file = "a.sql", "queries/a.sql"
	t1.editor.SetValue("")
	must(t, m.saveSession())
	r := newModel(m.proj)
	r.restoreSession()
	if r.cur().file != "queries/a.sql" || r.cur().editor.Value() != "" {
		t.Fatalf("지운 글은 비운 채로: %q", r.cur().editor.Value())
	}
}
