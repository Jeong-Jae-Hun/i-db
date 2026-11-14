package redisview

import (
	"errors"
	"reflect"
	"strings"
	"testing"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/x/ansi"
)

var specialKeys = map[string]tea.KeyType{
	"down": tea.KeyDown, "up": tea.KeyUp, "left": tea.KeyLeft, "right": tea.KeyRight,
	"enter": tea.KeyEnter, "esc": tea.KeyEsc, "backspace": tea.KeyBackspace, "delete": tea.KeyDelete,
	"home": tea.KeyHome, "end": tea.KeyEnd, " ": tea.KeySpace, "tab": tea.KeyTab, "ctrl+u": tea.KeyCtrlU,
	"pgdown": tea.KeyPgDown, "pgup": tea.KeyPgUp,
}

// press 는 키를 차례로 누르고 마지막 Action 을 돌려준다. 특수 키가 아닌 글자는 붙여넣기처럼 한 번에 넣는다.
func press(m Model, keys ...string) (Model, Action) {
	var a Action
	for _, k := range keys {
		msg := tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune(k)}
		if t, ok := specialKeys[k]; ok {
			msg = tea.KeyMsg{Type: t}
		}
		m, a = m.Update(msg)
	}
	return m, a
}

func sample(typ string) Value {
	return Value{
		Key: "k:1", Type: typ, TTL: -1,
		String:   "hello",
		Fields:   [][2]string{{"name", "kim"}, {"age", "3"}},
		Items:    []string{"x", "y", "x"},
		Members:  []string{"m1", "m2"},
		ZMembers: [][2]string{{"z1", "1.5"}, {"z2", "2"}},
	}
}

func newModel(v Value) Model { return NewEditor(v).SetSize(60, 12) }

func TestEditor_키마다_정확한_Redis_인자를_만든다(t *testing.T) {
	withTTL := sample("string")
	withTTL.TTL = 90 * time.Second
	cases := []struct {
		name string
		v    Value
		keys []string
		want [][]string
		key  string
	}{
		{"string 편집", sample("string"), []string{"e", "ctrl+u", "새 값", "enter"}, [][]string{{"SET", "k:1", "새 값"}}, "k:1"},
		{"TTL 있는 string 편집은 KEEPTTL", withTTL, []string{"enter", " 끝", "enter"}, [][]string{{"SET", "k:1", "hello 끝", "KEEPTTL"}}, "k:1"},
		{"hash 값 편집", sample("hash"), []string{"down", "e", "backspace", "4", "enter"}, [][]string{{"HSET", "k:1", "age", "4"}}, "k:1"},
		{"hash 추가는 덮어쓰지 않는다", sample("hash"), []string{"a", "city", "enter", "서울", "enter"}, [][]string{{"HSETNX", "k:1", "city", "서울"}}, "k:1"},
		{"hash 삭제", sample("hash"), []string{"d", "y"}, [][]string{{"HDEL", "k:1", "name"}}, "k:1"},
		{"list 편집은 읽은 값일 때만 인덱스로", sample("list"), []string{"G", "e", "ctrl+u", "z", "enter"}, [][]string{{"EVAL", listSetScript, "1", "k:1", "2", "x", "z"}}, "k:1"},
		{"list 추가", sample("list"), []string{"a", "w", "enter"}, [][]string{{"RPUSH", "k:1", "w"}}, "k:1"},
		{"set 편집은 추가 뒤 제거", sample("set"), []string{"j", "e", "ctrl+u", "m3", "enter"}, [][]string{{"SADD", "k:1", "m3"}, {"SREM", "k:1", "m2"}}, "k:1"},
		{"set 추가", sample("set"), []string{"a", "m9", "enter"}, [][]string{{"SADD", "k:1", "m9"}}, "k:1"},
		{"set 삭제", sample("set"), []string{"d", "y"}, [][]string{{"SREM", "k:1", "m1"}}, "k:1"},
		{"zset 점수 편집", sample("zset"), []string{"e", "ctrl+u", "1e3", "enter"}, [][]string{{"ZADD", "k:1", "XX", "1000", "z1"}}, "k:1"},
		{"zset 추가", sample("zset"), []string{"a", "z3", "enter", "-inf", "enter"}, [][]string{{"ZADD", "k:1", "NX", "-inf", "z3"}}, "k:1"},
		{"zset 삭제", sample("zset"), []string{"j", "d", "y"}, [][]string{{"ZREM", "k:1", "z2"}}, "k:1"},
		{"TTL 설정", sample("hash"), []string{"t", "60", "enter"}, [][]string{{"EXPIRE", "k:1", "60"}}, "k:1"},
		{"TTL 비우면 PERSIST", withTTL, []string{"t", "ctrl+u", "enter"}, [][]string{{"PERSIST", "k:1"}}, "k:1"},
		{"이름 바꾸기는 RENAMENX", sample("set"), []string{"r", "ctrl+u", "k:2", "enter"}, [][]string{{"RENAMENX", "k:1", "k:2"}}, "k:2"},
		{"키 삭제", sample("stream"), []string{"D", "y"}, [][]string{{"DEL", "k:1"}}, ""},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			m, a := press(newModel(c.v), c.keys...)
			if !reflect.DeepEqual(a.Run, c.want) {
				t.Fatalf("Run %q, want %q (status %q)", a.Run, c.want, a.Status)
			}
			if a.Key != c.key || a.Status == "" || m.Editing() {
				t.Fatalf("Key %q Status %q Editing %v", a.Key, a.Status, m.Editing())
			}
		})
	}
}

func TestEditor_list_삭제는_읽은_값일_때만_고른_칸을_표식으로_바꿔_한_스크립트에서_지운다(t *testing.T) {
	_, a := press(newModel(sample("list")), "G", "d", "y")
	if len(a.Run) != 1 {
		t.Fatalf("Run %q", a.Run)
	}
	ev := a.Run[0]
	if len(ev) != 7 || ev[0] != "EVAL" || ev[1] != listDeleteScript || ev[2] != "1" || ev[3] != "k:1" || ev[4] != "2" || ev[5] != "x" {
		t.Fatalf("Run %q", a.Run)
	}
	if !strings.HasPrefix(ev[6], sentinelPrefix) || len(ev[6]) != len(sentinelPrefix)+32 {
		t.Fatalf("표식 형식이 틀리다: %q", ev[6])
	}
	seen := map[string]bool{}
	for range 200 {
		run, err := build(edit{op: opDelete, typ: "list", key: "k", index: 0})
		if err != nil {
			t.Fatal(err)
		}
		if seen[run[0][6]] {
			t.Fatalf("표식이 겹쳤다: %s", run[0][6])
		}
		seen[run[0][6]] = true
	}
}

func TestEditor_바이너리나_큰_값도_고른_칸이면_지울_수_있다(t *testing.T) {
	bin := sample("hash")
	bin.Fields[0][1] = "\xff\xfe"
	big := sample("set")
	big.Members[0] = strings.Repeat("a", MaxStringEdit+1)
	list := sample("list")
	list.Items[0] = "\xff"
	if _, a := press(newModel(bin), "d", "y"); !reflect.DeepEqual(a.Run, [][]string{{"HDEL", "k:1", "name"}}) {
		t.Fatalf("hash: %q %q", a.Run, a.Status)
	}
	if _, a := press(newModel(big), "d", "y"); !reflect.DeepEqual(a.Run, [][]string{{"SREM", "k:1", big.Members[0]}}) {
		t.Fatalf("set: %q", a.Status)
	}
	if _, a := press(newModel(list), "d", "y"); len(a.Run) != 1 || a.Run[0][5] != "\xff" {
		t.Fatalf("list: %q %q", a.Run, a.Status)
	}
	if _, a := press(newModel(bin), "e"); !strings.Contains(a.Status, "binary") {
		t.Fatalf("편집은 여전히 거부해야 한다: %q", a.Status)
	}
}

func TestEditor_삭제_확인에서_y가_아니면_취소한다(t *testing.T) {
	for _, k := range []string{"n", "esc", "Y", "enter"} {
		m, a := press(newModel(sample("hash")), "D", k)
		if a.Run != nil || a.Status != "취소" || m.Editing() {
			t.Fatalf("%q: %+v", k, a)
		}
	}
}

func TestEditor_esc는_입력을_취소하고_바뀐_것이_없으면_실행하지_않는다(t *testing.T) {
	m, a := press(newModel(sample("hash")), "e", "zzz", "esc")
	if a.Run != nil || a.Status != "취소" || m.Editing() {
		t.Fatalf("esc: %+v", a)
	}
	for _, keys := range [][]string{{"e", "enter"}, {"r", "enter"}} {
		_, a = press(newModel(sample("hash")), keys...)
		if a.Run != nil || a.Status != "변경 없음" {
			t.Fatalf("%v: %+v", keys, a)
		}
	}
	_, a = press(newModel(sample("hash")), "r", "ctrl+u", "enter")
	if a.Run != nil || !strings.Contains(a.Status, "empty") {
		t.Fatalf("빈 이름: %+v", a)
	}
}

func TestEditor_입력은_줄바꿈_탭_한글을_그대로_두고_커서_위치에_넣는다(t *testing.T) {
	_, a := press(newModel(sample("string")), "e", "home", "가\n나\t", "right", "delete", "end", "backspace", "!", "enter")
	want := [][]string{{"SET", "k:1", "가\n나\th" + "ll!"}}
	if !reflect.DeepEqual(a.Run, want) {
		t.Fatalf("Run %q, want %q", a.Run, want)
	}
}

func TestInput_끝과_처음에서_지우기는_무시하고_ctrl_u는_커서_앞을_지운다(t *testing.T) {
	in := input{buf: []rune("가나다"), pos: 0}
	in = in.update(tea.KeyMsg{Type: tea.KeyBackspace})
	in = in.update(tea.KeyMsg{Type: tea.KeyRight})
	in = in.update(tea.KeyMsg{Type: tea.KeyRight})
	prev := in
	in = in.update(tea.KeyMsg{Type: tea.KeyCtrlU})
	if string(in.buf) != "다" || in.pos != 0 || string(prev.buf) != "가나다" {
		t.Fatalf("ctrl+u: %q %d, 이전 사본 %q", string(in.buf), in.pos, string(prev.buf))
	}
	in = in.update(tea.KeyMsg{Type: tea.KeyEnd})
	in = in.update(tea.KeyMsg{Type: tea.KeyDelete})
	in = in.update(tea.KeyMsg{Type: tea.KeyRight})
	if string(in.buf) != "다" || in.pos != 1 {
		t.Fatalf("끝: %q %d", string(in.buf), in.pos)
	}
}

func TestEditor_고칠_수_없는_값은_거부한다(t *testing.T) {
	part := sample("string")
	part.Truncated = true
	big := sample("string")
	big.String = strings.Repeat("a", MaxStringEdit+1)
	bin := sample("hash")
	bin.Fields[0][1] = "\xff\xfe"
	empty := sample("list")
	empty.Items = nil
	cases := []struct {
		name string
		v    Value
		keys []string
		want string
	}{
		{"앞부분만 읽은 string", part, []string{"e"}, "partially"},
		{"1MiB 넘는 string", big, []string{"e"}, "larger"},
		{"바이너리 값", bin, []string{"e"}, "binary"},
		{"빈 list 삭제", empty, []string{"d"}, "no item"},
		{"string 은 d 대신 D", sample("string"), []string{"d"}, "D"},
		{"stream 편집", sample("stream"), []string{"e"}, "not supported"},
		{"stream 추가", sample("stream"), []string{"a"}, "not supported"},
		{"stream 항목 삭제", sample("stream"), []string{"d"}, "not supported"},
		{"0초 TTL", sample("hash"), []string{"t", "0", "enter"}, "positive"},
		{"NaN 점수", sample("zset"), []string{"e", "ctrl+u", "NaN", "enter"}, "NaN"},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			m, a := press(newModel(c.v), c.keys...)
			if a.Run != nil || !strings.Contains(a.Status, c.want) || m.Editing() {
				t.Fatalf("%+v Editing %v", a, m.Editing())
			}
		})
	}
	if _, err := build(edit{op: opAdd, typ: "list", key: "k", text: strings.Repeat("a", MaxStringEdit+1)}); !errors.Is(err, errTooLarge) {
		t.Fatalf("붙여넣은 큰 값: %v", err)
	}
}

func TestParseTTL_초_빈값_마이너스1을_읽고_나머지는_거부한다(t *testing.T) {
	cases := []struct {
		in      string
		secs    int64
		persist bool
		ok      bool
	}{
		{"60", 60, false, true},
		{" 3600 ", 3600, false, true},
		{"", 0, true, true},
		{"-1", 0, true, true},
		{"0", 0, false, false},
		{"-5", 0, false, false},
		{"1h", 0, false, false},
		{"99999999999999999999", 0, false, false},
	}
	for _, c := range cases {
		secs, persist, err := parseTTL(c.in)
		if (err == nil) != c.ok || secs != c.secs || persist != c.persist {
			t.Errorf("parseTTL(%q) = %d %v %v", c.in, secs, persist, err)
		}
		if err != nil && !errors.Is(err, errTTL) {
			t.Errorf("parseTTL(%q) 오류 종류 %v", c.in, err)
		}
	}
}

func TestParseScore_Redis_표기로_고친다(t *testing.T) {
	cases := map[string]string{"1": "1", "1.50": "1.5", "inf": "+inf", "+Inf": "+inf", "-inf": "-inf", "1e3": "1000", " 2 ": "2"}
	for in, want := range cases {
		if got, err := parseScore(in); err != nil || got != want {
			t.Errorf("parseScore(%q) = %q %v, want %q", in, got, err, want)
		}
	}
	for _, in := range []string{"", "abc", "nan"} {
		if _, err := parseScore(in); err == nil {
			t.Errorf("parseScore(%q) 는 실패해야 한다", in)
		}
	}
}

func TestEditor_커서는_항목_안에서_움직이고_스크롤이_따라간다(t *testing.T) {
	v := sample("list")
	v.Items = make([]string, 50)
	m := NewEditor(v).SetSize(30, 7) // 본문 5줄
	m, _ = press(m, "up")
	if m.sel != 0 {
		t.Fatalf("위 끝: %d", m.sel)
	}
	m, _ = press(m, "pgdown")
	if m.sel != 10 || m.top != 6 {
		t.Fatalf("pgdown: sel %d top %d", m.sel, m.top)
	}
	m, _ = press(m, "G")
	if m.sel != 49 || m.top != 45 {
		t.Fatalf("G: sel %d top %d", m.sel, m.top)
	}
	m, _ = press(m, "g")
	if m.sel != 0 || m.top != 0 {
		t.Fatalf("g: sel %d top %d", m.sel, m.top)
	}
	if !strings.Contains(ansi.Strip(m.View()), "▌ 0") {
		t.Fatalf("커서 줄:\n%s", m.View())
	}
}

func TestEditor_입력_중_상태와_두_단계_프롬프트(t *testing.T) {
	m, _ := press(newModel(sample("zset")), "a")
	if !m.Editing() || !strings.Contains(m.View(), "새 멤버") {
		t.Fatalf("첫 단계:\n%s", m.View())
	}
	m, _ = press(m, "z9", "enter")
	if !m.Editing() || !strings.Contains(m.View(), "점수: ▏") {
		t.Fatalf("두 번째 단계:\n%s", m.View())
	}
}
