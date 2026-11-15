// Package statcrypt 는 pgcrypto encrypt_iv 와 같은 방식의 정적 암호화(서버 함수 이름 관례상 staticEncrypt)가 만든 `<ivhex>^|^<datahex>` 값을
// 서버 함수를 부르지 않고 풀고(명세 R37) 만든다(R52). pgcrypto decrypt_iv(data, key::bytea, iv, 'aes') 와 같은 결과를 낸다.
package statcrypt

import (
	"bytes"
	"crypto/aes"
	"crypto/cipher"
	"encoding/hex"
	"errors"
	"regexp"
	"strings"
	"unicode/utf8"
)

const sep = "^|^"

// staticEncrypt 의 IV 는 16바이트(32 hex)이고, 암호문은 PKCS#7 로 채운 16바이트 블록이다
var encryptedRe = regexp.MustCompile(`^[0-9A-Fa-f]{32}\^\|\^(?:[0-9A-Fa-f]{32})+$`)

// LooksEncrypted 는 s 가 staticEncrypt 결과 꼴인지다. 키 없이 화면에서 풀 칸을 고를 때 쓴다.
func LooksEncrypted(s string) bool { return encryptedRe.MatchString(s) }

// Decrypt 는 cipherText 를 key 로 푼다. 오류에는 키·값을 싣지 않는다.
// 키는 SQL 의 '<key>'::bytea 처럼 bytea 입력 규칙으로 읽고(대개 텍스트 바이트 그대로), pgcrypto 처럼
// 32바이트를 넘으면 자르고 16·24·32 중 맞는 크기까지 0 으로 채운다. IV 도 16바이트로 0 채움·자름이다.
func Decrypt(cipherText, key string) (string, error) {
	// staticDecrypt 처럼 처음 나온 ^|^ 에서 나눈다
	ivHex, dataHex, ok := strings.Cut(cipherText, sep)
	if !ok {
		return "", errors.New("missing ^|^ separator")
	}
	ivRaw, err := hex.DecodeString(ivHex)
	if err != nil {
		return "", errors.New("invalid IV hex")
	}
	data, err := hex.DecodeString(dataHex)
	if err != nil {
		return "", errors.New("invalid ciphertext hex")
	}
	if len(data) == 0 || len(data)%aes.BlockSize != 0 {
		return "", errors.New("ciphertext length is not a multiple of the AES block size")
	}
	block, err := newBlock(key)
	if err != nil {
		return "", err
	}
	iv := make([]byte, aes.BlockSize)
	copy(iv, ivRaw)
	plain := make([]byte, len(data))
	cipher.NewCBCDecrypter(block, iv).CryptBlocks(plain, data)

	// 틀린 키는 대개 여기서 걸린다. pgcrypto 도 채움 바이트를 모두 대조한다
	pad := int(plain[len(plain)-1])
	if pad == 0 || pad > aes.BlockSize {
		return "", errors.New("bad padding: wrong key or corrupted data")
	}
	for _, b := range plain[len(plain)-pad:] {
		if int(b) != pad {
			return "", errors.New("bad padding: wrong key or corrupted data")
		}
	}
	out := plain[:len(plain)-pad]
	if !utf8.Valid(out) {
		return "", errors.New("decrypted value is not valid UTF-8: wrong key or not text")
	}
	return string(out), nil
}

// Encrypt 는 staticEncrypt 와 같은 값을 서버 함수 없이 만든다(명세 R52). 결과는 Postgres encode(…, 'hex') 처럼
// 소문자 hex 인 `<ivhex>^|^<datahex>` 이고, IV 는 받은 바이트 그대로 앞에 적고 암호화에는 Decrypt 처럼 16바이트로
// 0 채움·자름한 값을 쓴다. 오류에는 키·값을 싣지 않는다.
func Encrypt(plain, key, ivHex string) (string, error) {
	ivRaw, err := hex.DecodeString(ivHex)
	if err != nil {
		return "", errors.New("invalid IV hex")
	}
	block, err := newBlock(key)
	if err != nil {
		return "", err
	}
	pad := aes.BlockSize - len(plain)%aes.BlockSize
	data := append([]byte(plain), bytes.Repeat([]byte{byte(pad)}, pad)...)
	iv := make([]byte, aes.BlockSize)
	copy(iv, ivRaw)
	cipher.NewCBCEncrypter(block, iv).CryptBlocks(data, data)
	return hex.EncodeToString(ivRaw) + sep + hex.EncodeToString(data), nil
}

// newBlock 은 pgcrypto 'aes' 처럼 키를 32바이트까지 자르고 16·24·32 중 맞는 크기까지 0 으로 채운 AES 블록이다.
func newBlock(key string) (cipher.Block, error) {
	k, err := byteaKey(key)
	if err != nil {
		return nil, err
	}
	k = k[:min(len(k), 32)]
	size := 32
	if len(k) <= 16 {
		size = 16
	} else if len(k) <= 24 {
		size = 24
	}
	return aes.NewCipher(append(k, make([]byte, size-len(k))...))
}

// byteaKey 는 PostgreSQL byteain 과 같다. `\x` 로 시작하면 hex, 아니면 `\\` 는 백슬래시, `\ooo` 는 8진 바이트,
// 그 밖의 글자는 그대로다. 서버 함수는 흔히 키를 '<key>'::bytea 로 넘겨서 키에 백슬래시가 있으면 이 규칙이 적용된다.
func byteaKey(key string) ([]byte, error) {
	if rest, ok := strings.CutPrefix(key, `\x`); ok {
		// byteain 은 hex 자리 사이 공백을 허용한다
		b, err := hex.DecodeString(strings.Join(strings.Fields(rest), ""))
		if err != nil {
			return nil, errors.New(`invalid key: bad hex after \x`)
		}
		return b, nil
	}
	out := make([]byte, 0, len(key))
	for i := 0; i < len(key); i++ {
		switch {
		case key[i] != '\\':
			out = append(out, key[i])
		case i+1 < len(key) && key[i+1] == '\\':
			out = append(out, '\\')
			i++
		case i+3 < len(key) && key[i+1] >= '0' && key[i+1] <= '3' && isOctal(key[i+2]) && isOctal(key[i+3]):
			out = append(out, (key[i+1]-'0')<<6|(key[i+2]-'0')<<3|(key[i+3]-'0'))
			i += 3
		default:
			return nil, errors.New("invalid key: backslash must be followed by a backslash or three octal digits (bytea input rules)")
		}
	}
	return out, nil
}

func isOctal(b byte) bool { return b >= '0' && b <= '7' }
