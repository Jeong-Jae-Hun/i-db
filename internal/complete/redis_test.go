package complete

import (
	"reflect"
	"slices"
	"strings"
	"testing"
)

var testKeys = []string{"session:1", "session:2", "user:1", "my session", `q"k`}

func suggestRedis(text string) []Item {
	off := strings.IndexByte(text, '|')
	return SuggestRedis(Query{Text: text[:off] + text[off+1:], Offset: off}, testKeys)
}

func TestSuggestRedis_자리별_후보(t *testing.T) {
	cases := []struct {
		name string
		text string
		want []string
	}{
		{"첫 낱말은 명령", "hgeta|", []string{"HGETALL"}},
		{"접두 일치 뒤 부분 일치", "ttl|", []string{"TTL", "PTTL"}},
		{"빈 줄은 명령 전체", "|", nil},
		{"키 자리는 키", "GET sess|", []string{"session:1", "session:2", "my session"}},
		{"명령 대소문자 무시", "get u|", []string{"user:1"}},
		{"키 자리 아님", "SET k |", []string{}},
		{"DEL 은 모든 인자가 키", "DEL a b us|", []string{"user:1"}},
		{"RENAME 은 두 번째도 키", "RENAME a us|", []string{"user:1"}},
		{"RENAME 세 번째는 아님", "RENAME a b us|", []string{}},
		{"MSET 은 홀수 자리만 키", "MSET a 1 us|", []string{"user:1"}},
		{"MSET 짝수 자리는 값", "MSET a us|", []string{}},
		{"하위 명령", "CLIENT l|", []string{"LIST"}},
		{"MEMORY USAGE 키", "MEMORY USAGE us|", []string{"user:1"}},
		{"모르는 명령", "FOO us|", []string{}},
		{"따옴표 안 공백은 한 인자", `GET "my s|`, []string{"my session"}},
		{"커서 줄만 본다", "GET user:1\nTT|", []string{"TTL"}},
		{"커서 뒤는 무시", "GET us| whatever", []string{"user:1"}},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			got := labels(suggestRedis(c.text))
			if c.want == nil {
				if len(got) != maxItems {
					t.Fatalf("후보 %d개", len(got))
				}
				return
			}
			if len(got) < len(c.want) || !reflect.DeepEqual(got[:len(c.want)], c.want) || len(c.want) == 0 && len(got) != 0 {
				t.Fatalf("%q → %v, want 앞부분 %v", c.text, got, c.want)
			}
		})
	}
}

func TestSuggestRedis_삽입값(t *testing.T) {
	cases := []struct {
		text, label, kind, detail, insert string
		prefixLen                         int
	}{
		{"ge|", "GET", KindCommand, "key", "get", 2},
		{"GE|", "GET", KindCommand, "key", "GET", 2},
		{"GET my|", "my session", KindKey, "", `"my session"`, 2},
		{"GET q|", `q"k`, KindKey, "", `"q\"k"`, 1},
		{"GET us|", "user:1", KindKey, "", "user:1", 2},
	}
	for _, c := range cases {
		it, ok := find(suggestRedis(c.text), c.label)
		if !ok {
			t.Fatalf("%q 에 %q 없음", c.text, c.label)
		}
		if want := (Item{Label: c.label, Kind: c.kind, Detail: c.detail, Insert: c.insert, PrefixLen: c.prefixLen}); it != want {
			t.Fatalf("%q → %+v, want %+v", c.text, it, want)
		}
	}
}

func TestSuggestRedis_키는_대소문자를_가리고_전체로_맞춘다(t *testing.T) {
	keys := []string{"Foo", "foo", "user.profile", "profile:1"}
	if got := labels(SuggestRedis(Query{Text: "GET f", Offset: 5}, keys)); !reflect.DeepEqual(got, []string{"Foo", "foo", "user.profile", "profile:1"}) {
		t.Fatalf("f → %v", got)
	}
	if got := labels(SuggestRedis(Query{Text: "GET pro", Offset: 7}, keys)); !reflect.DeepEqual(got, []string{"profile:1", "user.profile"}) {
		t.Fatalf("pro → %v", got)
	}
}

func TestSuggestRedis_PrefixLen_은_바꿀_원문_바이트_수다(t *testing.T) {
	cases := []struct {
		text, label string
		want        int
	}{
		{"GET user:1|", "user:1", 6},
		{`GET "my k|`, "my key", 5},
		{`GET "my s|`, "my session", 5},
		{"GET |", "user:1", 0},
		{"get|", "GET", 3},
		{"  DEL a  us|", "user:1", 2},
	}
	for _, c := range cases {
		keys := append(slices.Clone(testKeys), "my key")
		off := strings.IndexByte(c.text, '|')
		got := SuggestRedis(Query{Text: c.text[:off] + c.text[off+1:], Offset: off}, keys)
		if it, ok := find(got, c.label); !ok || it.PrefixLen != c.want {
			t.Fatalf("%q → %+v, want PrefixLen %d", c.text, it, c.want)
		}
	}
}

func TestQuoteArg_SplitArgs_규칙으로_되살아난다(t *testing.T) {
	cases := map[string]string{"a": "a", "a b": `"a b"`, "": `""`, `a\b`: `"a\\b"`, "it's": `"it's"`, "탭\t": "\"탭\t\""}
	for in, want := range cases {
		if got := quoteArg(in); got != want {
			t.Fatalf("%q → %q, want %q", in, got, want)
		}
	}
}

func FuzzSuggestRedis_패닉없이_50개_이하(f *testing.F) {
	for _, s := range []string{"GET a", `GET "a b`, `SET 'x\`, "MSET a 1 b", "\n\n", "세션 키"} {
		f.Add(s, len(s))
	}
	f.Fuzz(func(t *testing.T, text string, offset int) {
		if got := SuggestRedis(Query{Text: text, Offset: offset}, testKeys); len(got) > maxItems {
			t.Fatalf("후보 %d개", len(got))
		}
	})
}
