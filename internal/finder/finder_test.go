package finder

import (
	"fmt"
	"math/rand"
	"runtime"
	"slices"
	"strings"
	"testing"
	"time"
	"unicode"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/x/ansi"
)

func TestMatch_순위_연속과_낱말_경계를_앞세운다(t *testing.T) {
	cases := []struct {
		q     string
		names []string
		want  []string // 앞에서부터 이 순서여야 한다
	}{
		{"usr", []string{"audit_user_sessions", "users"}, []string{"users", "audit_user_sessions"}},
		{"ur", []string{"four", "user_roles"}, []string{"user_roles", "four"}},
		{"order", []string{"o_r_d_e_r_log", "orders"}, []string{"orders", "o_r_d_e_r_log"}},
		{"ur", []string{"userauth", "userRoles"}, []string{"userRoles", "userauth"}},
		{"USERS", []string{"Users", "my_users"}, []string{"Users", "my_users"}},
		{"회원", []string{"회사_원장", "회원정보"}, []string{"회원정보", "회사_원장"}},
		{"sess", []string{"session:42", "user:1:sess"}, []string{"session:42", "user:1:sess"}},
	}
	for _, c := range cases {
		var items []Item
		for _, n := range c.names {
			items = append(items, Item{Kind: KindTable, Name: n})
		}
		var got []string
		for _, h := range Match(items, c.q) {
			got = append(got, h.Item.Name)
		}
		if len(got) < len(c.want) || !slices.Equal(got[:len(c.want)], c.want) {
			t.Errorf("%q: got %q want 앞 %q", c.q, got, c.want)
		}
	}
}

func TestMatch_스키마를_포함해_맞추고_위치를_돌려준다(t *testing.T) {
	items := []Item{{Conn: "c", Kind: KindTable, Schema: "public", Name: "users"}, {Kind: KindView, Schema: "audit", Name: "logs"}}
	hits := Match(items, "pub.us")
	if len(hits) != 1 || hits[0].Item.Name != "users" || !slices.Equal(hits[0].Pos, []int{0, 1, 2, 6, 7, 8}) {
		t.Fatalf("%+v", hits)
	}
	if Target(items[0]) != "public.users" || Target(Item{Name: "k"}) != "k" {
		t.Fatal("Target")
	}
	if hits := Match(items, "zz"); len(hits) != 0 {
		t.Fatalf("없는 글자: %+v", hits)
	}
	if hits := Match(items, "u s"); len(hits) == 0 || hits[0].Item.Name != "users" || !slices.Equal(hits[0].Pos, []int{7, 8}) {
		t.Fatalf("공백은 무시: %+v", hits)
	}
}

func TestMatch_점수가_같으면_입력_순서를_지키고_50개까지(t *testing.T) {
	var items []Item
	for i := range 120 {
		items = append(items, Item{Name: fmt.Sprintf("t%03d_x", i)})
	}
	hits := Match(items, "x")
	if len(hits) != MaxMatches {
		t.Fatalf("len %d", len(hits))
	}
	for i, h := range hits {
		if h.Index != i {
			t.Fatalf("안정 정렬이 아니다: %d번째가 %d", i, h.Index)
		}
	}
	if empty := Match(items, " "); len(empty) != MaxMatches || empty[3].Index != 3 || empty[3].Pos != nil {
		t.Fatalf("빈 검색: %+v", empty[:4])
	}
	if hits := Match(nil, "a"); len(hits) != 0 {
		t.Fatal("빈 목록")
	}
}

func TestMatch_5만개도_키_입력마다_50ms_안에_끝난다(t *testing.T) {
	if raceOn {
		t.Skip("속도 측정은 -race 빌드에서 의미가 없다")
	}
	r := rand.New(rand.NewSource(1))
	words := []string{"user", "order", "payment", "session", "log", "audit", "item", "product", "회원", "주문", "event", "role"}
	items := make([]Item, 50000)
	for i := range items {
		items[i] = Item{Conn: "conn", Kind: KindTable, Schema: words[r.Intn(len(words))],
			Name: fmt.Sprintf("%s_%s_%d", words[r.Intn(len(words))], words[r.Intn(len(words))], i)}
	}
	q := ""
	for _, ch := range "user_ord" {
		q += string(ch)
		best := time.Hour
		for range 3 {
			start := time.Now()
			Match(items, q)
			best = min(best, time.Since(start))
		}
		if best > 50*time.Millisecond {
			t.Errorf("%q: %v", q, best)
		}
	}
}

func TestMatch_20만_키와_아주_긴_이름도_시간과_메모리가_묶인다(t *testing.T) {
	if raceOn {
		t.Skip("속도 측정은 -race 빌드에서 의미가 없다")
	}
	r := rand.New(rand.NewSource(1))
	words := []string{"user", "order", "payment", "session", "log", "audit", "item", "product", "회원", "주문", "event", "role"}
	items := make([]Item, 200000)
	for i := range items {
		items[i] = Item{Conn: "conn", Kind: KindRedisKey, Name: fmt.Sprintf("%s:%s:%s:%d",
			words[r.Intn(len(words))], words[r.Intn(len(words))], words[r.Intn(len(words))], i)}
	}
	items[7].Name = strings.Repeat("u", 200000)
	long := strings.Repeat("u", maxQueryRunes)
	for _, q := range []string{"u", "user", "user:ord", long} {
		best := time.Hour
		for range 3 {
			start := time.Now()
			Match(items, q)
			best = min(best, time.Since(start))
		}
		if best > 80*time.Millisecond {
			t.Errorf("%q: %v", q, best)
		}
	}
	// 20만 자 이름 × 200자 질의 표는 300MB 를 넘는다. 상한 덕에 한 번 검색이 몇 MB 안에 끝나야 한다
	var before, after runtime.MemStats
	runtime.ReadMemStats(&before)
	hits := Match(items[7:8], long)
	runtime.ReadMemStats(&after)
	if got := after.TotalAlloc - before.TotalAlloc; got > 4<<20 || len(hits) != 1 {
		t.Fatalf("alloc %d bytes, hits %d", got, len(hits))
	}
}

func TestModel_입력하고_고르고_닫는다(t *testing.T) {
	items := []Item{{Conn: "a", Kind: KindTable, Schema: "public", Name: "users"}, {Conn: "a", Kind: KindTable, Schema: "public", Name: "orders"},
		{Conn: "r", Kind: KindRedisKey, Name: "user:1"}}
	m := New(items)
	m.SetSize(60, 10)
	if len(m.hits) != 3 {
		t.Fatalf("처음엔 전부: %d", len(m.hits))
	}
	m = typeKeys(m, "user")
	if m.Query() != "user" || len(m.hits) != 2 {
		t.Fatalf("거르기: %q %d", m.Query(), len(m.hits))
	}
	prev := m
	m, _ = m.Update(tea.KeyMsg{Type: tea.KeyBackspace})
	m = typeKeys(m, "X")
	if prev.Query() != "user" || m.Query() != "useX" {
		t.Fatalf("이전 값이 바뀌었다: %q %q", prev.Query(), m.Query())
	}
	m, _ = m.Update(tea.KeyMsg{Type: tea.KeyCtrlU})
	m = typeKeys(m, "us")
	m, _ = m.Update(tea.KeyMsg{Type: tea.KeyDown})
	m, a := m.Update(tea.KeyMsg{Type: tea.KeyEnter})
	if a.Chosen == nil || *a.Chosen != m.hits[1].Item || !a.Close {
		t.Fatalf("enter: %+v", a)
	}
	if _, a := m.Update(tea.KeyMsg{Type: tea.KeyEsc}); !a.Close || a.Chosen != nil {
		t.Fatalf("esc: %+v", a)
	}
	none := typeKeys(m, "zzz")
	if _, a := none.Update(tea.KeyMsg{Type: tea.KeyEnter}); a.Chosen != nil || a.Close {
		t.Fatalf("후보 없을 때 enter: %+v", a)
	}
	if !strings.Contains(none.View(), "일치하는 객체 없음") {
		t.Fatal(none.View())
	}
	pasted, _ := New(items).Update(tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("us\ner\x1b"), Paste: true})
	if pasted.Query() != "us er" {
		t.Fatalf("붙여넣기: %q", pasted.Query())
	}
}

func TestModel_그리기_강조와_제어_문자_가림과_작은_크기(t *testing.T) {
	items := []Item{{Conn: "local", Kind: KindTable, Schema: "public", Name: "evil\x1b]52;c;AAAA\a"}, {Conn: "local", Kind: KindFunction, Name: "회원_조회"}}
	for i := range 200 {
		items = append(items, Item{Conn: "c", Kind: KindView, Name: fmt.Sprintf("v%d", i)})
	}
	m := New(items)
	m.SetSize(50, 8)
	v := m.View()
	if strings.Contains(v, "\x1b]52") || strings.Contains(v, "\a") || !strings.Contains(v, `evil\x1b]52;c;AAAA\x07`) {
		t.Fatalf("제어 문자:\n%s", v)
	}
	m = typeKeys(m, "회조")
	if v := m.View(); !strings.Contains(v, "조회") || !strings.Contains(v, "1/202") {
		t.Fatalf("한글:\n%s", v)
	}
	big := New(items)
	for _, sz := range [][2]int{{0, 0}, {1, 1}, {3, 2}, {4, 3}, {10, 4}, {50, 8}, {200, 40}, {-1, -5}} {
		big.SetSize(sz[0], sz[1])
		big, _ = big.Update(tea.KeyMsg{Type: tea.KeyPgDown})
		big, _ = big.Update(tea.KeyMsg{Type: tea.KeyDown})
		v := big.View()
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
	big = New(items)
	big.SetSize(40, 10)
	big, _ = big.Update(tea.KeyMsg{Type: tea.KeyPgDown})
	big, _ = big.Update(tea.KeyMsg{Type: tea.KeyPgDown})
	if big.cur != 14 || big.cur < big.top || big.cur >= big.top+big.listHeight() {
		t.Fatalf("스크롤: cur %d top %d", big.cur, big.top)
	}
}

func FuzzMatch_패닉없이_위치는_질의_글자를_차례로_가리킨다(f *testing.F) {
	f.Add("public", "users", "pub.us")
	f.Add("", "세션:키", "세키")
	f.Add("ΣΑΣ", "İstanbul", "iσ")
	f.Add("a", "\xff\xfe", "\xff")
	f.Fuzz(func(t *testing.T, schema, name, q string) {
		it := Item{Schema: schema, Name: name}
		hits := Match([]Item{it}, q)
		var qr []rune
		for _, r := range q {
			if !unicode.IsSpace(r) {
				qr = append(qr, unicode.ToLower(r))
			}
		}
		if len(qr) == 0 {
			if len(hits) != 1 {
				t.Fatalf("빈 질의는 그대로: %d", len(hits))
			}
			return
		}
		if len(hits) == 0 {
			return
		}
		target := []rune(Target(it))
		pos := hits[0].Pos
		if len(pos) != len(qr) {
			t.Fatalf("pos %v query %q", pos, string(qr))
		}
		for i, p := range pos {
			if p < 0 || p >= len(target) || (i > 0 && p <= pos[i-1]) || unicode.ToLower(target[p]) != qr[i] {
				t.Fatalf("pos %v target %q query %q", pos, string(target), string(qr))
			}
		}
		m := New([]Item{it})
		m.SetSize(20, 5)
		m, _ = m.Update(tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune(q)})
		_ = m.View()
	})
}

func typeKeys(m Model, s string) Model {
	for _, r := range s {
		m, _ = m.Update(tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune{r}})
	}
	return m
}
