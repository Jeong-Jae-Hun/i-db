package statcrypt

import (
	"context"
	"crypto/aes"
	"crypto/cipher"
	"encoding/hex"
	"math/rand/v2"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/jackc/pgx/v5"
)

const (
	iv    = "0123456789abcdef0123456789abcdef"
	key32 = "0123456789abcdef0123456789abcdef"
)

// 아래 암호문은 PostgreSQL 16 pgcrypto 의 encrypt_iv(convert_to(p, 'UTF8'), k::bytea, iv, 'aes') 결과다
func TestDecrypt_pgcrypto_가_만든_값을_같은_글로_푼다(t *testing.T) {
	cases := []struct{ name, key, data, want string }{
		{"32바이트 키", key32, "da859a23e6f8c4c3662bdca5b43ee9d5", "010-1234-5678"},
		{"빈 글", key32, "63a31fabc4d385fb4f916e528f63ccf5", ""},
		{"16바이트보다 짧은 키는 0 으로 채운다", "short", "aa54d45a5abdf88f45dc861f2307dbb2", "홍길동"},
		{"20바이트 키는 24바이트로 채운다", "abcdefghijklmnopqrst", "ce748b1c60c70ecd55f92d30ba5cefe3", "x"},
		{"32바이트를 넘는 키는 자른다", key32 + "EXTRA", "cf8deb067d36582700c8a6f987e7d47c", "long key"},
		{"백슬래시 키는 bytea 이스케이프로 읽는다", `a\\b\101`, "70433b4716df00a8cc8bf92fa4fe7824", "bs"},
		{`\x 로 시작하는 키는 hex 로 읽는다`, `\x6b6579`, "c94b7ed9255efb565b0f945e746bada1", "hexkey"},
	}
	for _, c := range cases {
		got, err := Decrypt(strings.ToUpper(iv)+sep+c.data, c.key)
		if err != nil || got != c.want {
			t.Errorf("%s: got %q, %v want %q", c.name, got, err, c.want)
		}
	}
	// 짧은 IV 도 pgcrypto 처럼 0 으로 채운다: encrypt_iv('x', 'abc', '\x0102', 'aes')
	if got, err := Decrypt("0102"+sep+"98072268514e601c107743c2701503ab", "abc"); err != nil || got != "x" {
		t.Errorf("짧은 IV: %q %v", got, err)
	}
}

func TestDecrypt_틀린_키와_깨진_값은_패닉_없이_오류이고_키를_싣지_않는다(t *testing.T) {
	good := iv + sep + "da859a23e6f8c4c3662bdca5b43ee9d5"
	cases := map[string]struct{ in, key string }{
		"틀린 키":        {good, "wrong-key-wrong-key-wrong-key-xx"},
		"구분자 없음":      {iv + "da859a23e6f8c4c3662bdca5b43ee9d5", key32},
		"IV hex 아님":   {"zz" + sep + "da859a23e6f8c4c3662bdca5b43ee9d5", key32},
		"암호문 hex 아님":  {iv + sep + "xyz", key32},
		"홀수 hex":      {iv + sep + "da8", key32},
		"블록 크기 아님":    {iv + sep + "da859a23", key32},
		"빈 암호문":       {iv + sep, key32},
		"잘못된 bytea 키": {good, `bad\key`},
		"잘못된 hex 키":   {good, `\xzz`},
		"끝이 잘린 8진":    {good, `ab\10`},
	}
	for name, c := range cases {
		got, err := Decrypt(c.in, c.key)
		if err == nil {
			t.Errorf("%s: 오류여야 한다, got %q", name, got)
			continue
		}
		if strings.Contains(err.Error(), c.key) {
			t.Errorf("%s: 오류에 키가 있다: %v", name, err)
		}
	}
}

func TestDecrypt_채움은_맞아도_UTF8_이_아니면_오류다(t *testing.T) {
	// 0xff 0xfe 뒤에 PKCS#7 채움 14바이트를 붙여 AES-128(키 "k" 를 0 으로 채움)·CBC 로 암호화한다
	block, err := aes.NewCipher(append([]byte("k"), make([]byte, 15)...))
	if err != nil {
		t.Fatal(err)
	}
	plain := append([]byte{0xff, 0xfe}, []byte(strings.Repeat("\x0e", 14))...)
	ivRaw, _ := hex.DecodeString(iv)
	data := make([]byte, len(plain))
	cipher.NewCBCEncrypter(block, ivRaw).CryptBlocks(data, plain)
	if got, err := Decrypt(iv+sep+hex.EncodeToString(data), "k"); err == nil || !strings.Contains(err.Error(), "UTF-8") {
		t.Fatalf("UTF-8 아님: %q %v", got, err)
	}
}

func TestLooksEncrypted_IV_32자와_블록_단위_hex_만_맞다(t *testing.T) {
	cases := map[string]bool{
		iv + sep + "da859a23e6f8c4c3662bdca5b43ee9d5":        true,
		strings.ToUpper(iv) + sep + strings.Repeat("ab", 32): true,
		iv + sep + "da859a23":                                false,
		iv[:30] + sep + "da859a23e6f8c4c3662bdca5b43ee9d5":   false,
		iv + sep: false,
		iv + "^|" + "da859a23e6f8c4c3662bdca5b43ee9d5":                                           false,
		iv + sep + "da859a23e6f8c4c3662bdca5b43ee9dg":                                            false,
		" " + iv + sep + "da859a23e6f8c4c3662bdca5b43ee9d5":                                      false,
		iv + sep + "da859a23e6f8c4c3662bdca5b43ee9d5" + sep + "00000000000000000000000000000000": false,
		"010-1234-5678": false,
	}
	for in, want := range cases {
		if got := LooksEncrypted(in); got != want {
			t.Errorf("%q: got %v want %v", in, got, want)
		}
	}
}

// ─── 실제 pgcrypto 와 대조 ───

// pgCryptoConn 은 DBTUI_TEST_PG_URL 이 있을 때 pgcrypto 를 켠 연결이다.
func pgCryptoConn(t *testing.T) (*pgx.Conn, context.Context) {
	t.Helper()
	url := os.Getenv("DBTUI_TEST_PG_URL")
	if url == "" {
		t.Skip("DBTUI_TEST_PG_URL not set")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	t.Cleanup(cancel)
	conn, err := pgx.Connect(ctx, url)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = conn.Close(context.Background()) })
	if _, err := conn.Exec(ctx, "create extension if not exists pgcrypto"); err != nil {
		t.Fatal(err)
	}
	return conn, ctx
}

func TestEncrypt_Decrypt_무작위_글과_키를_pgcrypto_staticEncrypt_와_대조한다(t *testing.T) {
	conn, ctx := pgCryptoConn(t)
	const chars = "abcXYZ019 -_!@#$%^&*()=+[]{};:'\",.<>/?`~"
	words := []string{"홍길동", "서울시 강남구", "😀", "é", "\n\t", ""}
	rng := rand.New(rand.NewPCG(1, 2))
	for i := range 200 {
		var key strings.Builder
		for range 1 + rng.IntN(40) {
			key.WriteByte(chars[rng.IntN(len(chars))])
		}
		var plain strings.Builder
		for range rng.IntN(12) {
			if rng.IntN(3) == 0 {
				plain.WriteString(words[rng.IntN(len(words))])
			} else {
				plain.WriteByte(chars[rng.IntN(len(chars))])
			}
		}
		// examples/crypto.sql 의 staticEncrypt 본문과 같은 식이다
		var enc string
		err := conn.QueryRow(ctx, `select encode(decode($3, 'hex'), 'hex') || '^|^' ||
			encode(encrypt_iv(convert_to($1, 'UTF-8')::bytea, $2::text::bytea, decode($3, 'hex'), 'aes'), 'hex')`,
			plain.String(), key.String(), iv).Scan(&enc)
		if err != nil {
			t.Fatalf("%d: %v", i, err)
		}
		if !LooksEncrypted(enc) {
			t.Fatalf("%d: staticEncrypt 꼴이 아니다: %q", i, enc)
		}
		// 같은 값을 Go 로 만들면 바이트까지 같아야 한다(R52)
		if mine, err := Encrypt(plain.String(), key.String(), iv); err != nil || mine != enc {
			t.Fatalf("%d: key len %d: Encrypt %q, %v want %q", i, key.Len(), mine, err, enc)
		}
		got, err := Decrypt(enc, key.String())
		if err != nil || got != plain.String() {
			t.Fatalf("%d: key len %d: got %q, %v want %q", i, key.Len(), got, err, plain.String())
		}
		// 서버 staticDecrypt 와 같은 식으로 다시 풀어 Go 와 같은지도 본다
		var back string
		if err := conn.QueryRow(ctx, `select convert_from(decrypt_iv(decode(split_part($1, '^|^', 2), 'hex'),
			$2::text::bytea, decode(split_part($1, '^|^', 1), 'hex'), 'aes'), 'UTF-8')`, enc, key.String()).Scan(&back); err != nil || back != got {
			t.Fatalf("%d: 서버 복호화 %q %v", i, back, err)
		}
	}
}

func TestExpandEnc_리터럴을_PostgreSQL_과_같은_값으로_읽는다(t *testing.T) {
	conn, ctx := pgCryptoConn(t)
	lits := []string{`'a@b.com'`, `'it''s'`, `E'a\nb\tc'`, `E'\x41\x4a\101\7'`, `E'é\U0001F600'`, `E'\'q\\z\q'`, `e'x\xzz'`, `'한글 '' 따옴표'`}
	for _, lit := range lits {
		out, n, _, err := ExpandEnc("select enc("+lit+")", key32, iv)
		if err != nil || n != 1 {
			t.Fatalf("%s: %v n=%d", lit, err, n)
		}
		var same bool
		err = conn.QueryRow(ctx, out+` = encode(decode($2, 'hex'), 'hex') || '^|^' ||
			encode(encrypt_iv(convert_to(`+lit+`, 'UTF-8'), $1::text::bytea, decode($2, 'hex'), 'aes'), 'hex')`, key32, iv).Scan(&same)
		if err != nil || !same {
			t.Errorf("%s: 서버 값과 다르다 (%q) %v", lit, out, err)
		}
	}
}
