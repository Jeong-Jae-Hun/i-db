package redisview

import (
	"fmt"
	"strings"
	"testing"
	"time"

	"github.com/charmbracelet/x/ansi"
)

// names 는 마디 이름을 "이름/종류(개수)" 로 늘어놓아 비교하기 쉽게 한다.
func names(nodes []Node) []string {
	out := make([]string, len(nodes))
	for i, n := range nodes {
		kind := "d"
		if n.IsKey {
			kind = "k"
		}
		out[i] = fmt.Sprintf("%s/%s%d", n.Name, kind, n.Count)
	}
	return out
}

// allFolders 는 모든 폴더를 펼친 expanded 표를 만든다.
func allFolders(nodes []Node, into map[string]bool) map[string]bool {
	for _, n := range nodes {
		if !n.IsKey {
			into[n.Full] = true
			allFolders(n.Children, into)
		}
	}
	return into
}

func TestBuildTree_콜론으로_묶고_폴더를_먼저_자연정렬하며_개수를_센다(t *testing.T) {
	nodes := BuildTree([]string{"user:10", "user:2", "user:1:name", "session:a", "zeta", "alpha"})
	got := strings.Join(names(nodes), " ")
	if want := "session/d1 user/d3 alpha/k1 zeta/k1"; got != want {
		t.Fatalf("최상위 %q, want %q", got, want)
	}
	user := nodes[1]
	if user.Full != "user:" {
		t.Fatalf("폴더 Full %q", user.Full)
	}
	if got := strings.Join(names(user.Children), " "); got != "1/d1 2/k1 10/k1" {
		t.Fatalf("user 아래 %q", got)
	}
	if k := user.Children[2]; k.Full != "user:10" || !k.IsKey {
		t.Fatalf("키 Full %+v", k)
	}
}

func TestBuildTree_키이면서_접두어인_이름은_폴더와_키_둘다_나온다(t *testing.T) {
	nodes := BuildTree([]string{"a", "a:b"})
	if got := strings.Join(names(nodes), " "); got != "a/d1 a/k1" {
		t.Fatalf("got %q", got)
	}
	if nodes[0].Full != "a:" || nodes[1].Full != "a" {
		t.Fatalf("Full 이 겹친다: %q %q", nodes[0].Full, nodes[1].Full)
	}
}

func TestBuildTree_빈_조각과_끝_콜론과_빈_키도_잃지_않는다(t *testing.T) {
	nodes := BuildTree([]string{"a::b", "a:", ":", ""})
	if got := strings.Join(names(nodes), " "); got != "/d1 a/d2 /k1" {
		t.Fatalf("최상위 %q", got)
	}
	a := nodes[1]
	if got := strings.Join(names(a.Children), " "); got != "/d1 /k1" {
		t.Fatalf("a 아래 %q", got)
	}
	if a.Children[0].Full != "a::" || a.Children[1].Full != "a:" || a.Children[0].Children[0].Full != "a::b" {
		t.Fatalf("빈 조각 Full: %+v", a.Children)
	}
	if nodes[0].Children[0].Full != ":" || nodes[2].Full != "" {
		t.Fatalf("':' 과 빈 키: %+v %+v", nodes[0], nodes[2])
	}
}

func TestBuildTree_한글_키와_중복_키(t *testing.T) {
	nodes := BuildTree([]string{"세션:사용자:1", "세션:사용자:1", "세션:관리자"})
	if len(nodes) != 1 || nodes[0].Name != "세션" || nodes[0].Count != 2 {
		t.Fatalf("got %+v", names(nodes))
	}
	if got := strings.Join(names(nodes[0].Children), " "); got != "사용자/d1 관리자/k1" {
		t.Fatalf("세션 아래 %q", got)
	}
}

func TestBuildTree_깊이_제한을_넘는_조각은_키_이름에_남긴다(t *testing.T) {
	key := strings.Repeat("x:", maxDepth+3) + "end"
	nodes := BuildTree([]string{key})
	depth := 0
	for len(nodes) == 1 && !nodes[0].IsKey {
		nodes = nodes[0].Children
		depth++
	}
	if depth != maxDepth || nodes[0].Full != key || nodes[0].Name != "x:x:x:end" {
		t.Fatalf("깊이 %d, 잎 %+v", depth, nodes[0])
	}
}

func TestFlatten_펼친_폴더만_깊이와_함께_늘어놓는다(t *testing.T) {
	nodes := BuildTree([]string{"a:b:c", "a:d", "e"})
	rows := Flatten(nodes, map[string]bool{"a:": true})
	var got []string
	for _, r := range rows {
		got = append(got, fmt.Sprintf("%d%s%v", r.Depth, r.Full, r.Expanded))
	}
	if want := "0a:true 1a:b:false 1a:dfalse 0efalse"; strings.Join(got, " ") != want {
		t.Fatalf("got %q", strings.Join(got, " "))
	}
	if len(Flatten(nodes, nil)) != 2 {
		t.Fatal("expanded 가 nil 이면 최상위만 나와야 한다")
	}
}

func TestBuildTree_키_1000개를_빨리_묶는다(t *testing.T) {
	keys := make([]string, 1000)
	for i := range keys {
		keys[i] = fmt.Sprintf("svc%d:user:%d:field%d", i%7, i, i%13)
	}
	start := time.Now()
	nodes := BuildTree(keys)
	rows := Flatten(nodes, allFolders(nodes, map[string]bool{}))
	if el := time.Since(start); el > 200*time.Millisecond {
		t.Fatalf("너무 느리다: %v", el)
	}
	total, keyRows := 0, 0
	for _, n := range nodes {
		total += n.Count
	}
	for _, r := range rows {
		if r.IsKey {
			keyRows++
		}
	}
	if total != 1000 || keyRows != 1000 {
		t.Fatalf("개수 %d, 키 줄 %d", total, keyRows)
	}
}

func TestNaturalCompare_숫자는_크기로_나머지는_바이트로_비교한다(t *testing.T) {
	cases := []struct {
		a, b string
		want int
	}{
		{"2", "10", -1},
		{"user2", "user10", -1},
		{"a", "a1", -1},
		{"007", "7", -1}, // 수가 같으면 바이트 순으로 고정
		{"b", "a", 1},
		{"가", "나", -1},
		{"x9y", "x9y", 0},
		{"99999999999999999999999", "100000000000000000000000", -1}, // int64 를 넘는 수
	}
	for _, c := range cases {
		if got := naturalCompare(c.a, c.b); got != c.want {
			t.Errorf("naturalCompare(%q, %q) = %d, want %d", c.a, c.b, got, c.want)
		}
		if got := naturalCompare(c.b, c.a); got != -c.want {
			t.Errorf("뒤집은 naturalCompare(%q, %q) = %d, want %d", c.b, c.a, got, -c.want)
		}
	}
}

func TestCompareNodes_폴더가_같은_이름의_키보다_앞이다(t *testing.T) {
	if compareNodes(Node{Name: "z"}, Node{Name: "a", IsKey: true}) >= 0 {
		t.Fatal("폴더가 키 앞이어야 한다")
	}
}

func TestRenderTree_제어문자를_보이게_하고_한글_폭과_작은_크기를_지킨다(t *testing.T) {
	rows := Flatten(BuildTree([]string{"한글:키\x1b[31m", "plain"}), map[string]bool{"한글:": true})
	if l := RowLabel(rows[1]); strings.ContainsRune(l, 0x1b) || !strings.Contains(l, `키\x1b[31m`) {
		t.Fatalf("제어 문자: %q", l)
	}
	if l := RowLabel(Row{Name: "", IsKey: true}); l != `  ""` {
		t.Fatalf("빈 이름: %q", l)
	}
	for _, size := range [][2]int{{0, 0}, {1, 1}, {3, 2}, {10, 1}, {40, 10}} {
		out := RenderTree(rows, Viewport{Width: size[0], Height: size[1], Cursor: 2})
		lines := strings.Split(out, "\n")
		if out != "" && len(lines) > size[1] {
			t.Fatalf("%v: 줄 수 %d", size, len(lines))
		}
		for _, l := range lines {
			if w := ansi.StringWidth(l); w > size[0] {
				t.Fatalf("%v: 폭 %d %q", size, w, l)
			}
		}
	}
	out := RenderTree(rows, Viewport{Width: 30, Height: 1, Cursor: 2})
	if !strings.Contains(out, "▌") || !strings.Contains(out, "plain") {
		t.Fatalf("커서 줄이 보여야 한다: %q", out)
	}
	if out := RenderTree(nil, Viewport{Width: 20, Height: 3}); !strings.Contains(out, "키 없음") {
		t.Fatalf("빈 트리: %q", out)
	}
}

func FuzzBuildTree_모든_키가_한_번씩_키_마디로_남는다(f *testing.F) {
	for _, s := range []string{"a:b", "a::b", "a:", ":", "", "세션:1", "a:a:a:a:a:a:a:a:a:a:a"} {
		f.Add(s, "a")
	}
	f.Fuzz(func(t *testing.T, k1, k2 string) {
		keys := []string{k1, k2, k1 + ":" + k2, k2 + ":"}
		uniq := map[string]bool{}
		for _, k := range keys {
			uniq[k] = true
		}
		nodes := BuildTree(keys)
		total := 0
		for _, n := range nodes {
			total += n.Count
		}
		seen := map[string]int{}
		for _, r := range Flatten(nodes, allFolders(nodes, map[string]bool{})) {
			if r.IsKey {
				seen[r.Full]++
			}
			_ = RowLabel(r)
		}
		if total != len(uniq) || len(seen) != len(uniq) {
			t.Fatalf("개수 %d, 키 %d, want %d", total, len(seen), len(uniq))
		}
		for k := range uniq {
			if seen[k] != 1 {
				t.Fatalf("키 %q 가 %d 번 나왔다", k, seen[k])
			}
		}
	})
}
