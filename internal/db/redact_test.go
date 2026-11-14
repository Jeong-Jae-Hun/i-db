package db

import (
	"net/url"
	"strings"
	"testing"
)

func TestRedact_원문과_URL_인코딩과_소문자_퍼센트를_지운다(t *testing.T) {
	secret := "p@+ss/:?"
	for _, leaked := range []string{
		secret,
		url.QueryEscape(secret),
		url.PathEscape(secret),
		strings.TrimPrefix(url.UserPassword("u", secret).String(), "u:"),
		strings.ToLower(url.QueryEscape(secret)),
	} {
		if got := Redact("x "+leaked+" y", secret); got != "x *** y" {
			t.Fatalf("%q → %q", leaked, got)
		}
	}
	if got := Redact("x y", ""); got != "x y" {
		t.Fatalf("빈 비밀번호는 그대로: %q", got)
	}
}

func TestRedact_퍼센트_16진수_대소문자가_섞여도_지운다(t *testing.T) {
	for _, leaked := range []string{"p%3a%2F%3f", "p%3A%2f%3F", "p:/?"} {
		if got := Redact("x "+leaked+" y", "p:/?"); got != "x *** y" {
			t.Errorf("%q → %q", leaked, got)
		}
	}
}

func TestMaskSecrets_비밀번호를_싣는_문장은_글_전체를_가린다(t *testing.T) {
	for _, s := range []string{
		"ALTER ROLE app WITH PASSWORD 'hunter2';",
		"select 1;\ncreate user u encrypted password E'x';",
		"CREATE ROLE r LOGIN PASSWORD $$x$$",
		"AUTH default hunter2",
		"  auth hunter2",
		"ACL SETUSER bob on >hunter2",
		"CONFIG SET requirepass hunter2",
		"config set masterauth hunter2",
		"MIGRATE h 6379 k 0 5000 AUTH hunter2",
		"MIGRATE h 6379 k 0 5000 AUTH2 u hunter2",
		"HELLO 3 AUTH u hunter2",
		"GET a\nAUTH hunter2",
	} {
		if got := MaskSecrets(s); got != MaskedSQL {
			t.Errorf("%q → %q", s, got)
		}
	}
}

func TestMaskSecrets_비밀값이_없는_글은_그대로_둔다(t *testing.T) {
	for _, s := range []string{
		"SELECT password_hash FROM users",
		"select 'auth' as x",
		"GET auth",
		"CONFIG GET maxmemory",
		"MIGRATE h 6379 k 0 5000",
	} {
		if got := MaskSecrets(s); got != s {
			t.Errorf("%q → %q", s, got)
		}
	}
}
