package statcrypt

import (
	"math/rand/v2"
	"strings"
	"testing"
)

// 아래 암호문은 statcrypt_test.go 의 pgcrypto 값이다: staticEncrypt('010-1234-5678'), 키 key32
const phoneCipher = iv + sep + "da859a23e6f8c4c3662bdca5b43ee9d5"

func TestEncrypt_pgcrypto_와_같은_소문자_hex_를_만든다(t *testing.T) {
	got, err := Encrypt("010-1234-5678", key32, strings.ToUpper(iv))
	if err != nil || got != phoneCipher {
		t.Fatalf("got %q, %v want %q", got, err, phoneCipher)
	}
	// 짧은 IV 는 받은 바이트만 앞에 적고 암호화에는 0 으로 채운 IV 를 쓴다: encrypt_iv('x', 'abc', '\x0102', 'aes')
	if got, err := Encrypt("x", "abc", "0102"); err != nil || got != "0102"+sep+"98072268514e601c107743c2701503ab" {
		t.Fatalf("짧은 IV: %q %v", got, err)
	}
}

func TestEncrypt_Decrypt_로_되돌린다(t *testing.T) {
	rng := rand.New(rand.NewPCG(3, 4))
	words := []string{"a@b.com", "홍길동", "😀", "", strings.Repeat("x", 16), strings.Repeat("y", 31)}
	keys := []string{"k", key32, key32 + "EXTRA", "abcdefghijklmnopqrst", `a\\b\101`, `\x6b6579`}
	for i := range 100 {
		plain, key := words[rng.IntN(len(words))], keys[rng.IntN(len(keys))]
		enc, err := Encrypt(plain, key, iv)
		if err != nil {
			t.Fatalf("%d: %v", i, err)
		}
		if !LooksEncrypted(enc) {
			t.Fatalf("%d: staticEncrypt 꼴이 아니다: %q", i, enc)
		}
		if got, err := Decrypt(enc, key); err != nil || got != plain {
			t.Fatalf("%d: got %q, %v want %q", i, got, err, plain)
		}
	}
}

func TestEncrypt_잘못된_IV_와_키는_오류이고_키를_싣지_않는다(t *testing.T) {
	cases := map[string]struct{ key, iv string }{
		"IV hex 아님":   {key32, "zz"},
		"잘못된 bytea 키": {`bad\key`, iv},
		"잘못된 hex 키":   {`\xzz`, iv},
	}
	for name, c := range cases {
		got, err := Encrypt("x", c.key, c.iv)
		if err == nil {
			t.Errorf("%s: 오류여야 한다, got %q", name, got)
			continue
		}
		if strings.Contains(err.Error(), c.key) {
			t.Errorf("%s: 오류에 키가 있다: %v", name, err)
		}
	}
}

func TestExpandEnc_문자열_주석_밖의_enc_만_암호문_리터럴로_바꾼다(t *testing.T) {
	q := "'" + phoneCipher + "'"
	cases := []struct {
		name, in, want string
		n              int
	}{
		{"기본", "select * from users where phone = enc('010-1234-5678')", "select * from users where phone = " + q, 1},
		{"대소문자·공백", "where a = ENC ( '010-1234-5678' ) and b = Enc('010-1234-5678')", "where a = " + q + " and b = " + q, 2},
		{"'' 이스케이프는 값으로 읽는다", "select enc('it''s')", "select '" + must(t, "it's") + "'", 1},
		{"E'' 이스케이프", `select enc(E'a\nb\x41\101\u00e9\'')`, "select '" + must(t, "a\nbAAé'") + "'", 1},
		{"문자열 안", "select 'enc(''x'')'", "select 'enc(''x'')'", 0},
		{"줄 주석 안", "select 1 -- enc('x')\n", "select 1 -- enc('x')\n", 0},
		{"블록 주석 안", "select /* enc('x') */ 1", "select /* enc('x') */ 1", 0},
		{"$$ 안", "do $$ begin perform enc('x'); end $$", "do $$ begin perform enc('x'); end $$", 0},
		{"따옴표 이름", `select "enc"('x')`, `select "enc"('x')`, 0},
		{"한정 이름", "select s.enc('x')", "select s.enc('x')", 0},
		{"다른 이름의 일부", "select myenc('x'), enc_x('y'), encode('z', 'hex')", "select myenc('x'), enc_x('y'), encode('z', 'hex')", 0},
		{"열 이름 enc", "select enc from t where enc = 1", "select enc from t where enc = 1", 0},
	}
	for _, c := range cases {
		got, n, server, err := ExpandEnc(c.in, key32, iv)
		if err != nil || got != c.want || n != c.n || server {
			t.Errorf("%s:\n got %q n=%d server=%v %v\nwant %q n=%d", c.name, got, n, server, err, c.want, c.n)
		}
	}
}

func must(t *testing.T, plain string) string {
	t.Helper()
	enc, err := Encrypt(plain, key32, iv)
	if err != nil {
		t.Fatal(err)
	}
	return enc
}

func TestExpandEnc_키나_IV_가_없으면_서버_staticEncrypt_로_바꾼다(t *testing.T) {
	in := "select * from users where email = enc( 'a@b.com' ) -- enc('x')"
	want := "select * from users where email = staticEncrypt( 'a@b.com' ) -- enc('x')"
	for _, kv := range [][2]string{{"", iv}, {key32, ""}, {"", ""}} {
		got, n, server, err := ExpandEnc(in, kv[0], kv[1])
		if err != nil || got != want || n != 1 || !server {
			t.Errorf("%q: got %q n=%d server=%v %v", kv, got, n, server, err)
		}
	}
	// 바꿀 enc 가 없으면 server 는 거짓이다
	if got, n, server, err := ExpandEnc("select 1", "", ""); got != "select 1" || n != 0 || server || err != nil {
		t.Errorf("enc 없음: %q %d %v %v", got, n, server, err)
	}
	// 서버 쪽으로 바꿀 때도 인자 꼴은 검사한다
	if _, _, _, err := ExpandEnc("select enc(col)", "", ""); err == nil {
		t.Error("서버 바꾸기에서도 리터럴이 아니면 오류여야 한다")
	}
}

func TestExpandEnc_잘못된_꼴은_위치를_알리고_키와_글을_싣지_않는다(t *testing.T) {
	cases := map[string]struct{ in, pos string }{
		"리터럴 아님":         {"select enc(email)", "1:8"},
		"빈 인자":           {"select\n  enc()", "2:3"},
		"리터럴 둘":          {"select enc('secret', 'b')", "1:8"},
		"닫는 괄호 없음":       {"select 한글, enc('secret'", "1:12"},
		"닫히지 않은 리터럴":     {"select enc('secret", "1:8"},
		"리터럴 뒤 연산":       {"select enc('secret' || 'x')", "1:8"},
		"모자란 유니코드 이스케이프": {`select enc(E'secret\u12')`, "1:8"},
	}
	for name, c := range cases {
		got, n, _, err := ExpandEnc(c.in, key32, iv)
		if err == nil {
			t.Errorf("%s: 오류여야 한다, got %q n=%d", name, got, n)
			continue
		}
		if !strings.Contains(err.Error(), "at "+c.pos+":") {
			t.Errorf("%s: 위치 %s 가 없다: %v", name, c.pos, err)
		}
		if strings.Contains(err.Error(), key32) || strings.Contains(err.Error(), "secret") {
			t.Errorf("%s: 오류에 키·글이 있다: %v", name, err)
		}
	}
	if _, _, _, err := ExpandEnc("select enc('x')", `bad\key`, iv); err == nil || strings.Contains(err.Error(), `bad\key`) {
		t.Errorf("잘못된 키: %v", err)
	}
}

func TestExpandEnc_테이블_함수_이름_자리의_enc_는_그대로_둔다(t *testing.T) {
	for _, sql := range []string{
		"insert into enc (a) values (1)",
		"create table enc (a int)",
		"create or replace function enc(t text) returns text as $$ select t $$ language sql",
		"drop procedure enc()",
		"update enc set a = 1",
		"select * from enc ('x')",
		"select * from t join enc ('x') on true",
		"insert INTO /* 주석 */ enc (a) values (1)",
	} {
		out, n, _, err := ExpandEnc(sql, key32, iv)
		if err != nil || n != 0 || out != sql {
			t.Errorf("%q → %q n=%d err=%v", sql, out, n, err)
		}
	}
	// 이름 자리 뒤의 호출은 그대로 바꾼다
	out, n, _, err := ExpandEnc("insert into enc (a) values (enc('010-1234-5678'))", key32, iv)
	if err != nil || n != 1 || out != "insert into enc (a) values ('"+phoneCipher+"')" {
		t.Fatalf("got %q n=%d err=%v", out, n, err)
	}
}
