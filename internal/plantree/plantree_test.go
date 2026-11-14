package plantree

import (
	"context"
	"math"
	"os"
	"strings"
	"testing"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/x/ansi"
	"github.com/jackc/pgx/v5"
)

// testdata/*.json 은 postgres:16 에서 EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) 으로 받은 실제 출력이다(estimate.json 만 ANALYZE 없음).

func TestParse_실제_픽스처를_모두_읽고_비율은_0에서_100_사이다(t *testing.T) {
	for _, name := range []string{"seqscan", "indexscan", "nestedloop", "hashjoin", "cte", "parallel", "estimate"} {
		root := fixture(t, name)
		if root.Analyzed != (name != "estimate") {
			t.Errorf("%s: Analyzed=%v", name, root.Analyzed)
		}
		var sum float64
		for _, n := range flatten(root) {
			if n.Percent < 0 || n.Percent > 100 || n.SelfTime < 0 || n.SelfCost < 0 {
				t.Errorf("%s %s: percent %v self %v cost %v", name, n.Type, n.Percent, n.SelfTime, n.SelfCost)
			}
			sum += n.Percent
		}
		// 자기 몫을 다 더하면 100% 다. CTE Scan 은 CTE 계산 시간을 빼서 두 번 세지 않는다
		if math.Abs(sum-100) > 0.5 {
			t.Errorf("%s: 비율 합 %.2f", name, sum)
		}
	}
}

func TestParse_노드_종류별_필드를_채운다(t *testing.T) {
	seq := fixture(t, "seqscan")
	if seq.Type != "Seq Scan" || seq.Relation != "pt_users" || !strings.Contains(seq.Filter, "u12") || seq.RemovedByFilter == 0 || seq.SharedHit == 0 {
		t.Errorf("seq scan: %+v", seq)
	}
	if seq.ExecutionTime <= 0 || seq.PlanningTime <= 0 || seq.Percent != 100 {
		t.Errorf("seq scan 시간: %+v", seq)
	}
	idx := fixture(t, "indexscan")
	if idx.Type != "Index Scan" || idx.Index != "pt_users_pkey" || !strings.Contains(idx.IndexCond, "id = 42") {
		t.Errorf("index scan: %+v", idx)
	}
	nl := fixture(t, "nestedloop")
	inner := nl.Children[1]
	if nl.Type != "Nested Loop" || inner.Loops != 19 || math.Abs(inner.TotalTime-inner.ActualTime*19) > 1e-9 {
		t.Errorf("nested loop 안쪽은 루프 수를 곱한다: %+v", inner)
	}
	hj := fixture(t, "hashjoin").Children[0]
	if hj.Type != "Hash Join" || hj.JoinType != "Inner" || !strings.Contains(hj.HashCond, "user_id") || hj.Children[1].Type != "Hash" {
		t.Errorf("hash join: %+v", hj)
	}
}

func TestParse_병렬_구간은_프로세스_수로_나눈다(t *testing.T) {
	gather := fixture(t, "parallel").Children[0]
	if gather.Type != "Gather" || gather.WorkersLaunched != 2 {
		t.Fatalf("gather: %+v", gather)
	}
	partial := gather.Children[0]
	if partial.Loops != 3 || math.Abs(partial.TotalTime-partial.ActualTime) > 1e-9 || partial.TotalTime > gather.TotalTime {
		t.Fatalf("워커 노드 시간은 벽시계 기준이어야 한다: %+v (gather %v)", partial, gather.TotalTime)
	}
}

func TestParse_CTE와_실행되지_않은_노드(t *testing.T) {
	root := fixture(t, "cte")
	initPlan := root.Children[0]
	if initPlan.Parent != "InitPlan" || initPlan.SubplanName != "CTE big" {
		t.Fatalf("InitPlan: %+v", initPlan)
	}
	join := root.Children[1].Children[0]
	scan, hash := join.Children[0], join.Children[1]
	if scan.Type != "CTE Scan" || scan.CTEName != "big" || !scan.Misestimated() || scan.Misestimate() >= 1 {
		t.Errorf("CTE Scan 은 실제 0행이라 과대 추정이다: %+v ratio %v", scan, scan.Misestimate())
	}
	if want := scan.TotalTime - initPlan.TotalTime; math.Abs(scan.SelfTime-want) > 1e-9 {
		t.Errorf("CTE Scan 자기 시간은 CTE 계산을 뺀 %v 여야 한다: %v", want, scan.SelfTime)
	}
	if scan.MisestimateMark() != "↓6675×" {
		t.Errorf("mark: %q", scan.MisestimateMark())
	}
	if hash.Loops != 0 || hash.Misestimate() != 0 || hash.Misestimated() {
		t.Errorf("never executed: %+v", hash)
	}
	if got := label(initPlan); got != "CTE big: Aggregate" {
		t.Errorf("label: %q", got)
	}
}

func TestParse_ANALYZE_없으면_비용으로_비율을_낸다(t *testing.T) {
	root := fixture(t, "estimate")
	if root.Analyzed || root.TotalTime != 0 {
		t.Fatalf("%+v", root)
	}
	for _, n := range flatten(root) {
		if n.Misestimated() || n.Self() != n.SelfCost {
			t.Errorf("%s: ANALYZE 없이 추정 오차를 낼 수 없다", n.Type)
		}
	}
}

func TestParse_객체만_와도_읽고_과소_추정을_잡는다(t *testing.T) {
	root, err := Parse(`{"Plan": {"Node Type": "Hash Join", "Join Type": "Left", "Total Cost": 10, "Plan Rows": 2,
		"Actual Total Time": 4, "Actual Rows": 50.5, "Actual Loops": 2, "Plans": [
			{"Node Type": "Seq Scan", "Relation Name": "a", "Alias": "x", "Total Cost": 4, "Plan Rows": 1, "Actual Total Time": 1, "Actual Rows": 0.5, "Actual Loops": 2}
		]}, "Planning Time": 0.1, "Execution Time": 8.2}`)
	if err != nil {
		t.Fatal(err)
	}
	if root.TotalTime != 8 || root.SelfTime != 6 || root.Percent != 75 || root.Children[0].Percent != 25 {
		t.Errorf("시간: total %v self %v pct %v/%v", root.TotalTime, root.SelfTime, root.Percent, root.Children[0].Percent)
	}
	if !root.Misestimated() || root.Misestimate() != 25.25 || root.MisestimateMark() != "↑25×" {
		t.Errorf("과소 추정: %v", root.Misestimate())
	}
	if got := label(root); got != "Hash Join (Left)" {
		t.Errorf("label: %q", got)
	}
	if got := label(root.Children[0]); got != "Seq Scan on a x" {
		t.Errorf("label: %q", got)
	}
}

func TestParse_계획이_없거나_JSON이_아니면_오류(t *testing.T) {
	for _, s := range []string{"", "[]", "{}", `[{"Plan": {}}]`, "Seq Scan on t", `[{"Plan": 1}]`} {
		if _, err := Parse(s); err == nil {
			t.Errorf("%q: 오류여야 한다", s)
		}
	}
}

func TestParse_넘치는_숫자도_비율은_0에서_100이고_그릴_수_있다(t *testing.T) {
	for _, s := range []string{
		`[{"Plan":{"Node Type":"X","Actual Total Time":1e308,"Actual Loops":1e308},"Execution Time":1}]`,
		`[{"Plan":{"Node Type":"X","Total Cost":1e308,"Plans":[{"Node Type":"Y","Total Cost":-1e308}]}}]`,
	} {
		root, err := Parse(s)
		if err != nil {
			t.Fatal(err)
		}
		for _, n := range flatten(root) {
			if !(n.Percent >= 0 && n.Percent <= 100) {
				t.Errorf("%s: percent %v", s, n.Percent)
			}
		}
		m := New(root)
		m.SetSize(40, 12)
		_ = m.View() // NaN 비율로 막대 칸 수가 음수가 되면 패닉이 났다
	}
}

func TestModel_펼침_접기와_이동(t *testing.T) {
	m := New(fixture(t, "hashjoin"))
	m.SetSize(100, 30)
	if len(m.rows) != 5 {
		t.Fatalf("rows %d", len(m.rows))
	}
	m = press(m, "enter")
	if len(m.rows) != 1 || !strings.Contains(m.View(), "▸ Aggregate") {
		t.Fatalf("접기: %d\n%s", len(m.rows), m.View())
	}
	m = press(m, " ")
	m = press(m, "j", "j", "j")
	if m.Selected().Type != "Hash" {
		t.Fatalf("이동: %s", m.Selected().Type)
	}
	m = press(m, "h") // 펼친 Hash 를 접는다
	if len(m.rows) != 4 {
		t.Fatalf("h 로 접기: %d", len(m.rows))
	}
	m = press(m, "h") // 접힌 노드에서 h 는 부모로
	if m.Selected().Type != "Hash Join" {
		t.Fatalf("부모로: %s", m.Selected().Type)
	}
	m = press(m, "l")
	if m.Selected().Type != "Seq Scan" {
		t.Fatalf("l 은 첫 자식으로: %s", m.Selected().Type)
	}
	m = press(m, "G", "l")
	if len(m.rows) != 5 || m.Selected().Type != "Hash" {
		t.Fatalf("l 로 펼치기: %d %s", len(m.rows), m.Selected().Type)
	}
	if _, a := m.Update(tea.KeyMsg{Type: tea.KeyEsc}); !a.Close {
		t.Fatal("esc 는 닫기")
	}
}

func TestModel_비싼_노드_3개를_강조하고_오차를_표시한다(t *testing.T) {
	root := fixture(t, "hashjoin")
	m := New(root)
	if len(m.rank) != topRanks {
		t.Fatalf("rank %d", len(m.rank))
	}
	var best *Node
	for _, n := range flatten(root) {
		if best == nil || n.SelfTime > best.SelfTime {
			best = n
		}
	}
	if m.rank[best] != 0 {
		t.Fatalf("1위가 자기 시간 최대 노드가 아니다: %s", best.Type)
	}
	c := New(fixture(t, "cte"))
	c.SetSize(120, 30)
	v := c.View()
	if !strings.Contains(v, "↓") || !strings.Contains(v, "never executed") || !strings.Contains(v, "execution 28.1ms") {
		t.Fatalf("view:\n%s", v)
	}
}

func TestModel_상세_칸과_제어_문자_가림(t *testing.T) {
	root, err := Parse(`[{"Plan": {"Node Type": "Seq Scan", "Relation Name": "t\u001b[31m", "Total Cost": 1, "Plan Rows": 1,
		"Filter": "(a = 'x\u001b]52;c;AAAA\u0007')", "Rows Removed by Filter": 3, "Shared Hit Blocks": 7}}]`)
	if err != nil {
		t.Fatal(err)
	}
	d := strings.Join(details(root), "\n")
	for _, s := range []string{`Seq Scan on t\x1b[31m`, `Filter: (a = 'x\x1b]52;c;AAAA\x07')`, "Rows Removed by Filter: 3", "buffers shared hit 7", "self cost 1.00 (100.0%)"} {
		if !strings.Contains(d, s) {
			t.Errorf("상세에 %q 가 없다:\n%s", s, d)
		}
	}
	m := New(root)
	m.SetSize(80, 12)
	if strings.Contains(m.View(), "\x1b]52") || strings.Contains(m.View(), "\a") {
		t.Fatal("제어 문자가 그대로 나간다")
	}
}

func TestModel_작은_크기에서도_패닉없이_크기를_지킨다(t *testing.T) {
	for _, name := range []string{"cte", "parallel"} {
		m := New(fixture(t, name))
		for _, sz := range [][2]int{{0, 0}, {1, 1}, {3, 2}, {10, 5}, {40, 9}, {40, 10}, {200, 60}, {-3, -1}} {
			m.SetSize(sz[0], sz[1])
			m = press(m, "G", "pgup", "pgdown", "k")
			v := m.View()
			if v == "" {
				if sz[0] > 0 && sz[1] > 0 {
					t.Errorf("%v: 빈 화면", sz)
				}
				continue
			}
			lines := strings.Split(v, "\n")
			if len(lines) > sz[1] {
				t.Errorf("%v: %d줄", sz, len(lines))
			}
			for _, ln := range lines {
				if w := ansi.StringWidth(ln); w > sz[0] {
					t.Errorf("%v: 폭 %d %q", sz, w, ln)
				}
			}
		}
	}
	var empty Model
	empty.SetSize(10, 10)
	empty, _ = empty.Update(tea.KeyMsg{Type: tea.KeyDown})
	if empty.View() != "" || New(nil).Selected() != nil {
		t.Fatal("빈 모델")
	}
}

func TestParse_실제_PostgreSQL_EXPLAIN_ANALYZE_JSON(t *testing.T) {
	url := os.Getenv("DBTUI_TEST_PG_URL")
	if url == "" {
		t.Skip("DBTUI_TEST_PG_URL not set")
	}
	c, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	conn, err := pgx.Connect(c, url+"?sslmode=disable")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if err := conn.Close(context.Background()); err != nil {
			t.Error(err)
		}
	})
	if _, err := conn.Exec(c, `create temp table pt (id int primary key, g int);
		insert into pt select i, i % 7 from generate_series(1, 5000) i; analyze pt`); err != nil {
		t.Fatal(err)
	}
	var out string
	err = conn.QueryRow(c, `EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)
		with s as materialized (select g, count(*) n from pt group by g)
		select * from pt join s using (g) where pt.id < 100 order by id`).Scan(&out)
	if err != nil {
		t.Fatal(err)
	}
	root, err := Parse(out)
	if err != nil {
		t.Fatal(err)
	}
	if !root.Analyzed || root.ExecutionTime <= 0 || root.TotalTime <= 0 {
		t.Fatalf("%+v", root)
	}
	var types []string
	for _, n := range flatten(root) {
		types = append(types, label(n))
	}
	if all := strings.Join(types, " | "); !strings.Contains(all, "CTE s:") || !strings.Contains(all, "on pt") {
		t.Fatalf("노드: %s", all)
	}
	m := New(root)
	m.SetSize(100, 20)
	if !strings.Contains(m.View(), "planning ") {
		t.Fatalf("view:\n%s", m.View())
	}
}

func FuzzParse_패닉없이_비율은_100을_넘지_않는다(f *testing.F) {
	// 큰 픽스처를 씨앗으로 두면 새 커버리지마다 입력 줄이기(minimize)가 오래 걸려 실행 수가 크게 준다
	for _, name := range []string{"seqscan", "indexscan"} {
		b, err := os.ReadFile("testdata/" + name + ".json")
		if err != nil {
			f.Fatal(err)
		}
		f.Add(string(b))
	}
	f.Add(`[{"Plan":{"Node Type":"Gather","Workers Launched":1,"Actual Total Time":2,"Actual Loops":1,"Plans":[{"Node Type":"CTE Scan","CTE Name":"a","Actual Total Time":1,"Actual Loops":2}]},"Execution Time":2}]`)
	f.Add(`[{"Plan":{"Node Type":"X","Actual Total Time":1e308,"Actual Loops":1e308},"Execution Time":1}]`)
	f.Add(`{"Plan":{"Node Type":"X","Actual Loops":-1,"Plans":[{"Node Type":"Y","Total Cost":1e308}]},"Execution Time":0}`)
	f.Fuzz(func(t *testing.T, s string) {
		root, err := Parse(s)
		if err != nil {
			return
		}
		for _, n := range flatten(root) {
			if !(n.Percent >= 0 && n.Percent <= 100) {
				t.Fatalf("percent %v", n.Percent)
			}
		}
		m := New(root)
		m.SetSize(30, 12)
		m, _ = m.Update(tea.KeyMsg{Type: tea.KeyEnter})
		_ = m.View()
	})
}

func fixture(t *testing.T, name string) *Node {
	t.Helper()
	b, err := os.ReadFile("testdata/" + name + ".json")
	if err != nil {
		t.Fatal(err)
	}
	root, err := Parse(string(b))
	if err != nil {
		t.Fatalf("%s: %v", name, err)
	}
	return root
}

func press(m Model, keys ...string) Model {
	for _, k := range keys {
		msg := tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune(k)}
		switch k {
		case "enter":
			msg = tea.KeyMsg{Type: tea.KeyEnter}
		case " ":
			msg = tea.KeyMsg{Type: tea.KeySpace, Runes: []rune(" ")}
		case "pgup":
			msg = tea.KeyMsg{Type: tea.KeyPgUp}
		case "pgdown":
			msg = tea.KeyMsg{Type: tea.KeyPgDown}
		}
		m, _ = m.Update(msg)
	}
	return m
}
