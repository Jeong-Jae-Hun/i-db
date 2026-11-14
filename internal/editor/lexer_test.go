package editor

import (
	"reflect"
	"testing"
)

type span struct {
	text string
	role role
}

func spans(src string, toks []token) []span {
	out := make([]span, 0, len(toks))
	for _, t := range toks {
		out = append(out, span{src[t.start:t.end], t.role})
	}
	return out
}

func TestLexSQL_키워드_문자열_숫자_주석을_나눈다(t *testing.T) {
	src := "SELECT count(*), t.type, 'it''s', 1.5e-3 FROM users t -- 끝\nWHERE id = $1::int"
	want := []span{
		{"SELECT", roleKeyword},
		{"count", roleFunc},
		{"*", roleOperator},
		{"'it''s'", roleString},
		{"1.5e-3", roleNumber},
		{"FROM", roleKeyword},
		{"-- 끝", roleComment},
		{"WHERE", roleKeyword},
		{"=", roleOperator},
		{"$1", roleParam},
		{"::", roleOperator},
		{"int", roleKeyword},
	}
	if got := spans(src, lexSQL(src)); !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v\nwant %v", got, want)
	}
}

func TestLexSQL_E문자열_따옴표_식별자_달러_인용(t *testing.T) {
	src := `E'a\'b' "My ""Tbl""" $fn$ select ';' $fn$ $$x$$`
	want := []span{
		{`E'a\'b'`, roleString},
		{`"My ""Tbl"""`, roleQuoted},
		{`$fn$ select ';' $fn$`, roleString},
		{`$$x$$`, roleString},
	}
	if got := spans(src, lexSQL(src)); !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v\nwant %v", got, want)
	}
}

func TestLexSQL_블록_주석은_중첩되고_닫히지_않으면_끝까지다(t *testing.T) {
	src := "/* a /* b */ c */ select /* 열림"
	want := []span{
		{"/* a /* b */ c */", roleComment},
		{"select", roleKeyword},
		{"/* 열림", roleComment},
	}
	if got := spans(src, lexSQL(src)); !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v\nwant %v", got, want)
	}
}

func TestLexSQL_닫히지_않은_문자열은_끝까지_문자열이다(t *testing.T) {
	src := "select 'abc\ndef"
	got := spans(src, lexSQL(src))
	if len(got) != 2 || got[1] != (span{"'abc\ndef", roleString}) {
		t.Fatalf("got %v", got)
	}
}

func TestLexRedis_첫_단어는_명령_따옴표는_문자열_숫자는_숫자(t *testing.T) {
	src := "SET \"a b\" 10\n  hget 'k\\'1' f"
	want := []span{
		{"SET", roleCommand},
		{`"a b"`, roleString},
		{"10", roleNumber},
		{"hget", roleCommand},
		{`'k\'1'`, roleString},
	}
	if got := spans(src, lexRedis(src)); !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v\nwant %v", got, want)
	}
}

func TestLexRedis_닫히지_않은_따옴표는_줄을_넘지_않는다(t *testing.T) {
	src := "GET \"abc\nDEL k"
	want := []span{
		{"GET", roleCommand},
		{`"abc`, roleString},
		{"DEL", roleCommand},
	}
	if got := spans(src, lexRedis(src)); !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v\nwant %v", got, want)
	}
}
