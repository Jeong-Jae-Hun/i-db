package main

import (
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

// ─── R74 키 안내 ───

func hintText(m *model) string { return strings.Join(m.hints(), " · ") }

// statusLine 은 화면 맨 아래 줄(상태 줄)이다.
func statusLine(m *model) string {
	lines := strings.Split(m.View(), "\n")
	return lines[len(lines)-1]
}

// keyTitles 는 ? 창이 고른 helpSections 묶음 제목이다.
func keyTitles(m *model) []string {
	var out []string
	for _, i := range m.keySections() {
		out = append(out, helpSections[i].title)
	}
	return out
}

func readOnlyResult() *db.Result {
	return &db.Result{Cols: []string{"id", "name"}, Rows: [][]string{{"1", "a"}, {"2", "b"}}}
}

func TestHints_고칠_수_있는_결과는_행_추가부터_보이고_a_가_실제로_행을_더한다(t *testing.T) {
	m := testModel(t)
	show(m, channels())
	m.setFocus(fResult)
	want := "a 행 추가 · Enter 고치기 · d 삭제 · Ctrl+D 복제 · Ctrl+K 복사 · Ctrl+B 연결된 곳 · s 정렬 · / 거르기 · ? 키"
	if got := hintText(m); got != want {
		t.Fatalf("편집 가능한 결과:\n got %q\nwant %q", got, want)
	}
	m.Update(key("a"))
	if m.cur().grid.Pending() != 1 {
		t.Fatal("안내한 a 는 행을 더한다")
	}
	want = "변경 1건 · Ctrl+S 제출 · u 되돌리기 · Enter 고치기 · a 행 추가 · d 삭제 · Ctrl+D 복제 · Ctrl+K 복사 · Ctrl+B 연결된 곳 · s 정렬 · / 거르기 · ? 키"
	if got := hintText(m); got != want {
		t.Fatalf("보류 중인 변경:\n got %q\nwant %q", got, want)
	}
}

func TestHints_읽기_전용_결과는_편집_키_없이_까닭을_짧게_보인다(t *testing.T) {
	m := testModel(t)
	show(m, readOnlyResult())
	m.setFocus(fResult)
	want := "s 정렬 · / 거르기 · Ctrl+K 복사 · Ctrl+B 연결된 곳 · v 레코드 · o 값 · 편집: PK 있는 단일 테이블만 · ? 키"
	if got := hintText(m); got != want {
		t.Fatalf("읽기 전용:\n got %q\nwant %q", got, want)
	}
}

func TestHints_Redis_결과는_연결된_곳과_편집_까닭을_보이지_않는다(t *testing.T) {
	m := testModel(t)
	m.proj.Conns = append(m.proj.Conns, db.Conn{Name: "r-stg", UUID: "r", Kind: db.KindRedis})
	m.Update(resultMsg{tab: m.cur().id, seq: m.cur().seq, conn: 2, res: readOnlyResult(), rev: m.cur().grid.Revision()})
	m.setFocus(fResult)
	if got := hintText(m); got != "s 정렬 · / 거르기 · Ctrl+K 복사 · v 레코드 · o 값 · ? 키" {
		t.Fatalf("Redis 결과: %q", got)
	}
}

func TestHints_잘린_PostgreSQL_결과는_더_읽기를_앞에_둔다(t *testing.T) {
	m := testModel(t)
	m.cur().conn = 1
	res := readOnlyResult()
	res.Truncated = true
	m.Update(resultMsg{tab: m.cur().id, seq: m.cur().seq, conn: 1, sql: "select id, name from t", res: res, rev: m.cur().grid.Revision()})
	m.setFocus(fResult)
	if got := hintText(m); !strings.HasPrefix(got, "] 더 읽기 · s 정렬") {
		t.Fatalf("잘린 결과: %q", got)
	}
	// 다시 읽을 수 없는 결과(편집기 실행이 아님)는 ] 가 듣지 않으므로 안내하지 않는다
	show(m, res)
	if got := hintText(m); strings.Contains(got, "더 읽기") {
		t.Fatalf("더 읽을 수 없는 결과: %q", got)
	}
}

func TestHints_선택_중에는_복사와_끝내기만_보인다(t *testing.T) {
	m := testModel(t)
	show(m, channels())
	m.setFocus(fResult)
	m.Update(key("V"))
	if got := hintText(m); got != "y 복사 · C IN 목록 · Ctrl+K 꼴 고르기 · esc 끝" {
		t.Fatalf("선택: %q", got)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	if strings.HasPrefix(hintText(m), "y 복사") {
		t.Fatal("선택을 끝내면 표 안내로 돌아간다")
	}
}

func TestHints_테이블_보기는_페이지_WHERE_정렬식을_먼저_보인다(t *testing.T) {
	m := testModel(t)
	tableResult(m)
	m.setFocus(fResult)
	want := "[ ] 페이지 · w WHERE · O 정렬식 · a 행 추가 · Enter 고치기 · d 삭제 · Ctrl+D 복제 · Ctrl+K 복사 · Ctrl+B 연결된 곳 · s 정렬 · ? 키"
	if got := hintText(m); got != want {
		t.Fatalf("테이블 보기:\n got %q\nwant %q", got, want)
	}
	m.Update(key("w"))
	if got := hintText(m); got != "Enter 적용 · esc 취소 · Tab 열 이름" {
		t.Fatalf("WHERE 입력 중: %q", got)
	}
}

func TestHints_거르기_입력과_칸_고치기는_확정_취소만_보이고_물음표는_글자다(t *testing.T) {
	m := testModel(t)
	show(m, channels())
	m.setFocus(fResult)
	m.Update(key("/"))
	if got := hintText(m); got != "Enter 확정 · esc 취소" {
		t.Fatalf("거르기 입력: %q", got)
	}
	m.Update(key("?"))
	if m.overlay != oNone || m.cur().grid.Query().Filter != "?" {
		t.Fatalf("거르기 입력의 ? 는 글자: overlay=%v filter=%q", m.overlay, m.cur().grid.Query().Filter)
	}
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	m.Update(key("enter"))
	if got := hintText(m); got != "Enter 확정 · esc 취소 · Ctrl+N NULL · Ctrl+O 큰 편집기" {
		t.Fatalf("칸 고치기: %q", got)
	}
	m.Update(key("?"))
	if m.overlay != oNone {
		t.Fatal("칸 고치기의 ? 는 글자")
	}
}

func TestHints_트리는_줄_종류에_따라_듣는_키만_보인다(t *testing.T) {
	m := testModel(t)
	m.proj.Conns = append(m.proj.Conns,
		db.Conn{Name: "mine-stg", UUID: "m", Kind: db.KindPG, Source: db.SourceDB},
		db.Conn{Name: "aws-stg", UUID: "a", Kind: db.KindAWS, Source: db.SourceDB})
	m.setFocus(fTree)
	cases := []struct {
		name string
		n    *node
		want string
	}{
		{"DataGrip 접속", &node{kind: nConn, conn: 1},
			"Enter 펼치기 · N 새 접속 · A/L/S 운영 · B DB · F 검색 · = 비교 · M 폴더 · r 새로고침 · ? 키"},
		{"db 전용 접속", &node{kind: nConn, conn: 2},
			"Enter 펼치기 · E 고치기 · N 새 접속 · X 지우기 · A/L/S 운영 · B DB · F 검색 · = 비교 · M 폴더 · r 새로고침 · ? 키"},
		{"테이블", &node{kind: nTable, conn: 1, label: "t"}, "Enter 열기 · d 구조 · D DDL · I CSV 가져오기 · / 거르기 · ? 키"},
		{"SNS 토픽", &node{kind: nSource, conn: 3, key: "sns/arn:aws:sns:ap-northeast-2:1:orders", openable: true, hasKids: true},
			"F 흐름 · Enter 미리 보기 · l 펼치기 · r 새로고침 · ? 키"},
		{"SQS 큐", &node{kind: nSource, conn: 3, key: "sqs/orders", openable: true}, "Enter 미리 보기 · r 새로고침 · ? 키"},
	}
	for _, c := range cases {
		m.rows, m.cursor = []row{{n: c.n}}, 0
		if got := hintText(m); got != c.want {
			t.Errorf("%s:\n got %q\nwant %q", c.name, got, c.want)
		}
	}
	m.filter = "x"
	m.rows = []row{{n: &node{kind: nTable, conn: 1}}}
	if got := hintText(m); !strings.HasPrefix(got, "esc 거르기 해제 · Enter 열기") {
		t.Fatalf("거르기 중: %q", got)
	}
}

func TestHints_편집기는_실행과_F1_을_보인다(t *testing.T) {
	m := testModel(t)
	m.setFocus(fEditor)
	if got := hintText(m); !strings.HasPrefix(got, "Ctrl+R 실행 · Ctrl+E 실행 계획 · F8 정렬 · F1 도움말") {
		t.Fatalf("편집기: %q", got)
	}
}

func TestHints_맥이면_키_안내가_커맨드_글자다(t *testing.T) {
	m := testModel(t)
	m.mac = true
	show(m, channels())
	m.setFocus(fResult)
	got := hintText(m)
	for _, want := range []string{"⌘D 복제", "⌘K 복사", "⌘B 연결된 곳"} {
		if !strings.Contains(got, want) {
			t.Errorf("%q 가 없다: %q", want, got)
		}
	}
	m.Update(key("a"))
	if got := hintText(m); !strings.HasPrefix(got, "변경 1건 · ⌘S 제출") || strings.Contains(got, "Ctrl+") {
		t.Fatalf("보류 중: %q", got)
	}
}

func TestFitHints_앞_항목부터_들어가는_만큼만_잇고_항목을_자르지_않는다(t *testing.T) {
	items := []string{"aa", "bb", "cc"}
	for w, want := range map[int]string{100: "aa · bb · cc", 7: "aa · bb", 6: "aa", 1: ""} {
		if got := fitHints(items, w); got != want {
			t.Errorf("폭 %d: %q, want %q", w, got, want)
		}
	}
}

func TestHints_좁은_화면의_상태_줄은_앞_항목만_남긴다(t *testing.T) {
	m := testModel(t)
	show(m, channels())
	m.setFocus(fResult)
	m.status = ""
	m.Update(tea.WindowSizeMsg{Width: 70, Height: 30})
	line := statusLine(m)
	full := hintText(m)
	if !strings.Contains(line, "a 행 추가 · Enter 고치기") || strings.Contains(line, "? 키") || strings.Contains(line, "…") {
		t.Fatalf("좁은 상태 줄: %q", line)
	}
	shown := strings.TrimSpace(line)
	if !strings.HasPrefix(full, shown) || !slices.Contains(m.hints(), shown[strings.LastIndex(shown, " · ")+len(" · "):]) {
		t.Fatalf("항목 중간에서 자르지 않는다: %q", shown)
	}
	m.Update(tea.WindowSizeMsg{Width: 200, Height: 30})
	if !strings.Contains(statusLine(m), full) {
		t.Fatal("넓으면 다 보인다")
	}
}

// ─── R74.2 이 화면의 키 ───

func TestKeysOverlay_결과에서_물음표는_결과_묶음만_보이고_esc_나_물음표로_닫는다(t *testing.T) {
	m := testModel(t)
	show(m, channels())
	m.setFocus(fResult)
	m.Update(key("?"))
	if m.overlay != oKeys {
		t.Fatal("결과에서 ? 는 이 화면의 키")
	}
	v := m.View()
	for _, want := range []string{"결과", "─ 편집", "변경 미리 보기 뒤 제출", "─ 복사·내보내기", "F1 전체 도움말"} {
		if !strings.Contains(v, want) {
			t.Errorf("%q 가 없다", want)
		}
	}
	if strings.Contains(v, "데이터베이스 바꾸기") || strings.Contains(v, "새 탭") {
		t.Error("트리·전역 묶음은 넣지 않는다")
	}
	m.Update(tea.WindowSizeMsg{Width: 200, Height: 60})
	if !strings.Contains(m.View(), "행 추가 · 복제 · 삭제 표시 · 되돌리기") {
		t.Error("넓은 창은 설명을 자르지 않는다")
	}
	m.Update(key("a"))
	if m.overlay != oKeys || m.cur().grid.Pending() != 0 {
		t.Fatal("다른 키는 창이 먹고 아무것도 하지 않는다")
	}
	m.Update(key("?"))
	if m.overlay != oNone {
		t.Fatal("? 로 닫는다")
	}
	m.Update(key("?"))
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	if m.overlay != oNone {
		t.Fatal("esc 로 닫는다")
	}
}

func TestKeysOverlay_포커스마다_helpSections_의_그_묶음을_고른다(t *testing.T) {
	m := testModel(t)
	m.setFocus(fTree)
	if got := keyTitles(m); !slices.Equal(got, []string{"트리"}) {
		t.Fatalf("트리: %v", got)
	}
	m.Update(key("?"))
	if v := m.View(); !strings.Contains(v, "새 접속 (URL 붙여넣기") || strings.Contains(v, "행 추가 · 복제") {
		t.Fatal("트리 창은 트리 키")
	}
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})

	m.cur().flow = &flowState{}
	m.setFocus(fEditor)
	if got := keyTitles(m); !slices.Equal(got, []string{"흐름 (Alt+F)"}) {
		t.Fatalf("흐름: %v", got)
	}
	m.Update(key("?"))
	if m.overlay != oKeys {
		t.Fatal("흐름 탭은 편집기 자리에서도 ? 가 창이다")
	}
	m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	m.cur().flow = nil
	m.cur().board = &boardState{}
	if got := keyTitles(m); !slices.Equal(got, []string{"보드 (Alt+B)"}) {
		t.Fatalf("보드: %v", got)
	}
	m.cur().board = nil

	m.setFocus(fEditor)
	m.Update(key("?"))
	if m.overlay != oNone || m.cur().editor.Value() != "?" {
		t.Fatal("편집기에서 ? 는 글자")
	}
}

func TestKeysOverlay_트리_거르기_입력의_물음표는_글자다(t *testing.T) {
	m := testModel(t)
	m.setFocus(fTree)
	m.Update(key("/"))
	m.Update(key("?"))
	if m.overlay != oFilter || m.input.Value() != "?" {
		t.Fatalf("overlay=%v input=%q", m.overlay, m.input.Value())
	}
}

// ─── R74.3 한 번만 보이는 팁 ───

func TestTips_처음_들어간_화면에서_한_번만_보이고_파일에_남는다(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("XDG_STATE_HOME", dir)
	t.Setenv("DB_TIPS", "")
	m := testModel(t)
	m.tips = loadTips()
	show(m, channels())
	m.setFocus(fResult)
	m.Update(key("j"))
	if !strings.Contains(statusLine(m), "팁: a 로 행 추가") {
		t.Fatalf("첫 편집 가능한 결과에서 팁: %q", statusLine(m))
	}
	m.Update(key("j"))
	if strings.Contains(statusLine(m), "팁:") {
		t.Fatal("다음 키에서 지우고 다시 보이지 않는다")
	}
	path := filepath.Join(dir, "db", "tips.json")
	st, err := os.Stat(path)
	if err != nil || st.Mode().Perm() != 0o600 {
		t.Fatalf("tips.json 0600: %v %v", err, st)
	}
	data, _ := os.ReadFile(path)
	if !strings.Contains(string(data), "result-edit") {
		t.Fatalf("본 팁을 남긴다: %s", data)
	}

	// 다음 실행도 본 팁은 보이지 않고, 처음 보는 읽기 전용 결과의 팁은 보인다
	m2 := testModel(t)
	m2.tips = loadTips()
	show(m2, channels())
	m2.setFocus(fResult)
	m2.Update(key("j"))
	if strings.Contains(statusLine(m2), "팁:") {
		t.Fatal("본 팁은 다시 보이지 않는다")
	}
	show(m2, readOnlyResult())
	if !strings.Contains(statusLine(m2), "팁: ? 로 이 화면의 키") {
		t.Fatalf("읽기 전용 결과 팁: %q", statusLine(m2))
	}
}

func TestTips_DB_TIPS_0_이면_끈다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	t.Setenv("DB_TIPS", "0")
	m := testModel(t)
	if m.tips = loadTips(); m.tips != nil {
		t.Fatal("DB_TIPS=0 이면 nil")
	}
	show(m, channels())
	m.setFocus(fResult)
	m.Update(key("j"))
	if strings.Contains(statusLine(m), "팁:") {
		t.Fatal("팁이 보이지 않는다")
	}
}

// ─── R73.4 읽기 접속 짝 ───

func TestLinkedConns_같은_DB_의_읽기_접속은_쓰기_접속이_있으면_뺀다(t *testing.T) {
	proj := &db.Project{Conns: []db.Conn{
		{Name: "app-stg", Group: "app", Kind: db.KindPG, DB: "app"},
		{Name: "app-stg-read", Group: "app", Kind: db.KindPG, DB: "app"},
		{Name: "app-ro", Group: "app", Kind: db.KindPG, DB: "app", Host: "x.cluster-ro-abc.rds.amazonaws.com"},
		{Name: "log-stg-read", Group: "app", Kind: db.KindPG, DB: "log", Host: "read.log"},
		{Name: "app2-stg", Group: "app", Kind: db.KindPG, DB: "app2"},
	}}
	if got := linkedConns(proj, 0); !slices.Equal(got, []int{0, 3, 4}) {
		t.Errorf("쓰기 접속이 있으면 같은 DB 읽기 접속은 뺀다, 짝 없는 읽기 접속은 남긴다: %v", got)
	}
	if got := linkedConns(proj, 1); !slices.Equal(got, []int{1, 3, 4}) {
		t.Errorf("읽기 접속에서 찾으면 그 접속은 남고 짝인 쓰기·다른 읽기 접속은 뺀다: %v", got)
	}
	if got := linkedConns(proj, 3); !slices.Equal(got, []int{3, 0, 4}) {
		t.Errorf("다른 DB 의 짝은 건드리지 않는다: %v", got)
	}
}
