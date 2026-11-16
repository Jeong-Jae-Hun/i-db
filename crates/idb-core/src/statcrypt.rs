//! pgcrypto encrypt_iv 와 같은 방식의 정적 암호화(서버 함수 이름 관례상 staticEncrypt)가 만든 `<ivhex>^|^<datahex>` 값을
//! 서버 함수를 부르지 않고 풀고(명세 R37) 만든다(R52). pgcrypto decrypt_iv(data, key::bytea, iv, 'aes') 와 같은 결과를 낸다.
//! Go `internal/statcrypt` 와 같다.

use aes::cipher::block_padding::NoPadding;
use aes::cipher::{BlockCipher, BlockDecryptMut, BlockEncryptMut, KeyInit, KeyIvInit};

use crate::sqlsafe::{go_lower, is_ident_byte, mask_bytes, quote_literal};

pub const SEP: &str = "^|^";

const BLOCK: usize = 16;

// ─── 암호문 꼴 ───

/// s 가 staticEncrypt 결과 꼴인지다. 키 없이 화면에서 풀 칸을 고를 때 쓴다.
/// IV 는 16바이트(32 hex)이고, 암호문은 PKCS#7 로 채운 16바이트 블록이다.
pub fn looks_encrypted(s: &str) -> bool {
    let b = s.as_bytes();
    let hex = |p: &[u8]| p.iter().all(u8::is_ascii_hexdigit);
    b.len() >= 32 + SEP.len() + 32 && hex(&b[..32]) && b[32..35] == *SEP.as_bytes() && {
        let data = &b[35..];
        data.len().is_multiple_of(32) && hex(data)
    }
}

// ─── 풀기·만들기 ───

/// cipher_text 를 key 로 푼다. 오류에는 키·값을 싣지 않는다.
/// 키는 SQL 의 '<key>'::bytea 처럼 bytea 입력 규칙으로 읽고(대개 텍스트 바이트 그대로), pgcrypto 처럼
/// 32바이트를 넘으면 자르고 16·24·32 중 맞는 크기까지 0 으로 채운다. IV 도 16바이트로 0 채움·자름이다.
pub fn decrypt(cipher_text: &str, key: &str) -> Result<String, &'static str> {
    // staticDecrypt 처럼 처음 나온 ^|^ 에서 나눈다
    let (iv_hex, data_hex) = cipher_text.split_once(SEP).ok_or("missing ^|^ separator")?;
    let iv_raw = hex_decode(iv_hex).ok_or("invalid IV hex")?;
    let mut data = hex_decode(data_hex).ok_or("invalid ciphertext hex")?;
    if data.is_empty() || !data.len().is_multiple_of(BLOCK) {
        return Err("ciphertext length is not a multiple of the AES block size");
    }
    let k = aes_key(key)?;
    let iv = pad_iv(&iv_raw);
    match k.len() {
        16 => cbc_decrypt::<aes::Aes128>(&k, &iv, &mut data),
        24 => cbc_decrypt::<aes::Aes192>(&k, &iv, &mut data),
        _ => cbc_decrypt::<aes::Aes256>(&k, &iv, &mut data),
    }

    // 틀린 키는 대개 여기서 걸린다. pgcrypto 도 채움 바이트를 모두 대조한다
    let pad = data[data.len() - 1] as usize;
    if pad == 0 || pad > BLOCK || data[data.len() - pad..].iter().any(|&b| b as usize != pad) {
        return Err("bad padding: wrong key or corrupted data");
    }
    data.truncate(data.len() - pad);
    String::from_utf8(data).map_err(|_| "decrypted value is not valid UTF-8: wrong key or not text")
}

/// staticEncrypt 와 같은 값을 서버 함수 없이 만든다(명세 R52). 결과는 Postgres encode(…, 'hex') 처럼
/// 소문자 hex 인 `<ivhex>^|^<datahex>` 이고, IV 는 받은 바이트 그대로 앞에 적고 암호화에는 `decrypt` 처럼 16바이트로
/// 0 채움·자름한 값을 쓴다. 오류에는 키·값을 싣지 않는다.
pub fn encrypt(plain: &str, key: &str, iv_hex: &str) -> Result<String, &'static str> {
    encrypt_bytes(plain.as_bytes(), key, iv_hex)
}

/// `encrypt` 와 같되 글이 UTF-8 이 아니어도 된다. E'\xff' 처럼 리터럴이 바이트를 만들 수 있어서다.
fn encrypt_bytes(plain: &[u8], key: &str, iv_hex: &str) -> Result<String, &'static str> {
    let iv_raw = hex_decode(iv_hex).ok_or("invalid IV hex")?;
    let k = aes_key(key)?;
    let pad = BLOCK - plain.len() % BLOCK;
    let mut data = plain.to_vec();
    data.resize(plain.len() + pad, pad as u8);
    let iv = pad_iv(&iv_raw);
    match k.len() {
        16 => cbc_encrypt::<aes::Aes128>(&k, &iv, &mut data),
        24 => cbc_encrypt::<aes::Aes192>(&k, &iv, &mut data),
        _ => cbc_encrypt::<aes::Aes256>(&k, &iv, &mut data),
    }
    Ok(format!("{}{SEP}{}", hex_encode(&iv_raw), hex_encode(&data)))
}

fn cbc_encrypt<C>(key: &[u8], iv: &[u8; BLOCK], data: &mut [u8])
where
    C: BlockEncryptMut + BlockCipher + KeyInit,
{
    let len = data.len();
    // 키 크기는 aes_key 가, 블록 배수는 호출부가 맞춘다
    cbc::Encryptor::<C>::new_from_slices(key, iv)
        .expect("AES key size")
        .encrypt_padded_mut::<NoPadding>(data, len)
        .expect("block multiple");
}

fn cbc_decrypt<C>(key: &[u8], iv: &[u8; BLOCK], data: &mut [u8])
where
    C: BlockDecryptMut + BlockCipher + KeyInit,
{
    cbc::Decryptor::<C>::new_from_slices(key, iv)
        .expect("AES key size")
        .decrypt_padded_mut::<NoPadding>(data)
        .expect("block multiple");
}

fn pad_iv(raw: &[u8]) -> [u8; BLOCK] {
    let mut iv = [0u8; BLOCK];
    let n = raw.len().min(BLOCK);
    iv[..n].copy_from_slice(&raw[..n]);
    iv
}

/// pgcrypto 'aes' 처럼 키를 32바이트까지 자르고 16·24·32 중 맞는 크기까지 0 으로 채운 AES 키다.
fn aes_key(key: &str) -> Result<Vec<u8>, &'static str> {
    let mut k = bytea_key(key)?;
    k.truncate(32);
    let size = match k.len() {
        0..=16 => 16,
        17..=24 => 24,
        _ => 32,
    };
    k.resize(size, 0);
    Ok(k)
}

/// PostgreSQL byteain 과 같다. `\x` 로 시작하면 hex, 아니면 `\\` 는 백슬래시, `\ooo` 는 8진 바이트,
/// 그 밖의 글자는 그대로다. 서버 함수는 흔히 키를 '<key>'::bytea 로 넘겨서 키에 백슬래시가 있으면 이 규칙이 적용된다.
fn bytea_key(key: &str) -> Result<Vec<u8>, &'static str> {
    if let Some(rest) = key.strip_prefix("\\x") {
        // byteain 은 hex 자리 사이 공백을 허용한다
        let joined: String = rest.split_whitespace().collect();
        return hex_decode(&joined).ok_or("invalid key: bad hex after \\x");
    }
    let k = key.as_bytes();
    let octal = |b: u8| (b'0'..=b'7').contains(&b);
    let mut out = Vec::with_capacity(k.len());
    let mut i = 0;
    while i < k.len() {
        if k[i] != b'\\' {
            out.push(k[i]);
        } else if i + 1 < k.len() && k[i + 1] == b'\\' {
            out.push(b'\\');
            i += 1;
        } else if i + 3 < k.len() && (b'0'..=b'3').contains(&k[i + 1]) && octal(k[i + 2]) && octal(k[i + 3]) {
            out.push((k[i + 1] - b'0') << 6 | (k[i + 2] - b'0') << 3 | (k[i + 3] - b'0'));
            i += 3;
        } else {
            return Err("invalid key: backslash must be followed by a backslash or three octal digits (bytea input rules)");
        }
        i += 1;
    }
    Ok(out)
}

/// Go `hex.DecodeString` 과 같다. 홀수 길이나 16진이 아닌 글자는 None 이다.
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if !b.len().is_multiple_of(2) {
        return None;
    }
    let nib = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    b.chunks(2).map(|p| Some(nib(p[0])? << 4 | nib(p[1])?)).collect()
}

fn hex_encode(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ─── enc() 펼치기 ───

/// 키·IV 가 없을 때 enc 를 바꿔 쓸 서버 함수다. db 는 만들지 않고 DB 에 이미 있을 때만 동작한다
const SERVER_FUNC: &str = "staticEncrypt";

/// `nameKeywords` 뒤의 enc( 는 호출이 아니라 테이블·함수 이름이다(insert into enc (a) …, create table enc (…)).
/// 한계: 목록 밖 자리(create table if not exists enc (…) 등)는 호출로 읽는다. 그때는 "enc" 나 schema.enc 로 쓴다.
const NAME_KEYWORDS: &[&str] = &["into", "from", "join", "update", "table", "function", "procedure"];

const SPACE_BYTES: &[u8] = b" \t\n\r\x0c\x0b";

/// 문자열·주석·$태그$ 밖의 enc('<글>') 호출을 그 글의 staticEncrypt 암호문 리터럴로 바꾼다(명세 R52).
/// 이름은 대소문자를 가리지 않고, 인자는 문자열 리터럴 하나(작은따옴표 두 번은 한 번, E'…' 도 받음)여야 한다.
/// key 나 iv_hex 가 비었으면 암호화하지 않고 enc 를 서버 함수 staticEncrypt 로 바꾸고 server 를 참으로 돌려준다.
/// 돌려주는 값은 (바꾼 SQL, 바꾼 호출 수, server) 다. 인자가 리터럴이 아니거나 닫히지 않았으면 그 위치(줄:칸)를 담은 오류이고,
/// 오류에는 키·글을 싣지 않는다. schema.enc(…)·"enc"(…) 처럼 한정·따옴표 이름과 into·from·join·update·table·function·procedure
/// 바로 뒤의 enc( (테이블·함수 이름 자리)는 건드리지 않는다.
pub fn expand_enc(sql: &str, key: &str, iv_hex: &str) -> Result<(String, usize, bool), String> {
    let server = key.is_empty() || iv_hex.is_empty();
    let m = mask_bytes(sql.as_bytes()).0;
    let mut b = String::new();
    let mut last = 0;
    let mut n = 0;
    let mut i = 0;
    while i + 3 <= m.len() {
        if !m[i..i + 3].eq_ignore_ascii_case(b"enc")
            || i > 0 && (is_ident_byte(m[i - 1]) || m[i - 1] == b'.')
            || i + 3 < m.len() && is_ident_byte(m[i + 3])
        {
            i += 1;
            continue;
        }
        let open = skip_space(&m, i + 3);
        if open >= m.len() || m[open] != b'(' || NAME_KEYWORDS.contains(&prev_word(&m, i).as_str()) {
            i += 1;
            continue; // enc 라는 열·별칭이거나 enc 라는 테이블·함수 이름이다
        }
        let mut q = skip_space(&m, open + 1);
        let escapes = q + 1 < m.len() && (m[q] == b'E' || m[q] == b'e') && m[q + 1] == b'\'';
        if escapes {
            q += 1;
        }
        if q >= m.len() || m[q] != b'\'' {
            return Err(pos_err(sql, i, "argument must be a single string literal"));
        }
        // 가린 사본은 리터럴 안을 공백으로 채우고 닫는 따옴표만 남긴다
        let Some(end) = m[q + 1..].iter().position(|&c| c == b'\'') else {
            return Err(pos_err(sql, i, "unterminated string literal"));
        };
        let end = end + q + 1;
        let close_paren = skip_space(&m, end + 1);
        if close_paren >= m.len() || m[close_paren] != b')' {
            return Err(pos_err(sql, i, "argument must be a single string literal followed by )"));
        }

        let repl = if server {
            format!("{SERVER_FUNC}{}", &sql[i + 3..=close_paren])
        } else {
            let plain = unquote(&sql[q + 1..end], escapes).map_err(|e| pos_err(sql, i, &e))?;
            let cipher = encrypt_bytes(&plain, key, iv_hex).map_err(|e| pos_err(sql, i, e))?;
            quote_literal(&cipher)
        };
        b.push_str(&sql[last..i]);
        b.push_str(&repl);
        last = close_paren + 1;
        i = close_paren + 1;
        n += 1;
    }
    if n == 0 {
        return Ok((sql.to_string(), 0, false));
    }
    b.push_str(&sql[last..]);
    Ok((b, n, server))
}

/// `sql[at]` 의 1부터 센 줄·칸(글자 단위)을 붙인 오류다.
fn pos_err(sql: &str, at: usize, msg: &str) -> String {
    let before = &sql[..at];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
    format!("enc() at {line}:{col}: {msg}")
}

/// `m[..i]` 끝의 공백을 건너뛴 바로 앞 단어(소문자)다. 단어가 아니면 빈 글이다.
fn prev_word(m: &[u8], i: usize) -> String {
    let mut j = i;
    while j > 0 && SPACE_BYTES.contains(&m[j - 1]) {
        j -= 1;
    }
    let mut k = j;
    while k > 0 && is_ident_byte(m[k - 1]) {
        k -= 1;
    }
    go_lower(&String::from_utf8_lossy(&m[k..j]))
}

fn skip_space(s: &[u8], mut i: usize) -> usize {
    while i < s.len() && SPACE_BYTES.contains(&s[i]) {
        i += 1;
    }
    i
}

/// 따옴표 안 글을 값으로 읽는다. 작은따옴표 두 번은 한 번이고, escapes(E'…')면 PostgreSQL 처럼 \b \f \n \r \t,
/// \ooo(8진 1~3자리), \xh(h)(16진 1~2자리), \uXXXX, \UXXXXXXXX 를 풀고 그 밖의 \c 는 c 다.
/// \u·\U 자리가 모자라면 PostgreSQL 처럼 오류다. \x 와 8진은 UTF-8 이 아닌 바이트도 만든다.
fn unquote(body: &str, escapes: bool) -> Result<Vec<u8>, String> {
    let body = body.replace("''", "'");
    if !escapes || !body.contains('\\') {
        return Ok(body.into_bytes());
    }
    let s = body.as_bytes();
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] != b'\\' || i + 1 == s.len() {
            out.push(s[i]);
            i += 1;
            continue;
        }
        i += 1;
        let c = s[i];
        match c {
            b'b' => out.push(b'\x08'),
            b'f' => out.push(b'\x0c'),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'x' | b'u' | b'U' => {
                let width = match c {
                    b'x' => 2,
                    b'u' => 4,
                    _ => 8,
                };
                let digits = s[i + 1..].iter().take(width).take_while(|d| d.is_ascii_hexdigit()).count();
                let j = i + 1 + digits;
                if c != b'x' && digits != width {
                    return Err(format!("invalid Unicode escape \\{}", c as char));
                }
                if digits == 0 {
                    // \x 뒤에 16진이 없으면 x 그대로다
                    out.push(c);
                    i += 1;
                    continue;
                }
                let v = u32::from_str_radix(std::str::from_utf8(&s[i + 1..j]).unwrap_or("0"), 16).unwrap_or(0);
                if c == b'x' {
                    out.push(v as u8);
                } else if let Some(r) = char::from_u32(v) {
                    out.extend_from_slice(r.encode_utf8(&mut [0; 4]).as_bytes());
                } else {
                    return Err(format!("invalid Unicode escape value \\{}", c as char));
                }
                i = j - 1;
            }
            b'0'..=b'7' => {
                let digits = s[i..].iter().take(3).take_while(|d| (b'0'..=b'7').contains(d)).count();
                let v = s[i..i + digits].iter().fold(0u32, |v, d| v * 8 + u32::from(d - b'0'));
                // Go byte(v) 처럼 8비트로 자른다(\777 → 0xff)
                out.push(v as u8);
                i += digits - 1;
            }
            _ => out.push(c),
        }
        i += 1;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IV: &str = "0123456789abcdef0123456789abcdef";
    const KEY32: &str = "0123456789abcdef0123456789abcdef";

    // ─── statcrypt_test.go ───

    // 아래 암호문은 PostgreSQL 16 pgcrypto 의 encrypt_iv(convert_to(p, 'UTF8'), k::bytea, iv, 'aes') 결과다
    #[test]
    fn pgcrypto_가_만든_값을_같은_글로_푼다() {
        let long = format!("{KEY32}EXTRA");
        let cases = [
            ("32바이트 키", KEY32, "da859a23e6f8c4c3662bdca5b43ee9d5", "010-1234-5678"),
            ("빈 글", KEY32, "63a31fabc4d385fb4f916e528f63ccf5", ""),
            (
                "16바이트보다 짧은 키는 0 으로 채운다",
                "short",
                "aa54d45a5abdf88f45dc861f2307dbb2",
                "홍길동",
            ),
            (
                "20바이트 키는 24바이트로 채운다",
                "abcdefghijklmnopqrst",
                "ce748b1c60c70ecd55f92d30ba5cefe3",
                "x",
            ),
            (
                "32바이트를 넘는 키는 자른다",
                long.as_str(),
                "cf8deb067d36582700c8a6f987e7d47c",
                "long key",
            ),
            (
                "백슬래시 키는 bytea 이스케이프로 읽는다",
                r"a\\b\101",
                "70433b4716df00a8cc8bf92fa4fe7824",
                "bs",
            ),
            (
                r"\x 로 시작하는 키는 hex 로 읽는다",
                r"\x6b6579",
                "c94b7ed9255efb565b0f945e746bada1",
                "hexkey",
            ),
        ];
        for (name, key, data, want) in cases {
            let got = decrypt(&format!("{}{SEP}{data}", IV.to_uppercase()), key);
            assert_eq!(got.as_deref(), Ok(want), "{name}");
        }
        // 짧은 IV 도 pgcrypto 처럼 0 으로 채운다: encrypt_iv('x', 'abc', '\x0102', 'aes')
        assert_eq!(
            decrypt(&format!("0102{SEP}98072268514e601c107743c2701503ab"), "abc").as_deref(),
            Ok("x"),
            "짧은 IV"
        );
    }

    #[test]
    fn 틀린_키와_깨진_값은_패닉_없이_오류이고_키를_싣지_않는다() {
        let good = format!("{IV}{SEP}da859a23e6f8c4c3662bdca5b43ee9d5");
        let cases = [
            ("틀린 키", good.clone(), "wrong-key-wrong-key-wrong-key-xx"),
            ("구분자 없음", format!("{IV}da859a23e6f8c4c3662bdca5b43ee9d5"), KEY32),
            ("IV hex 아님", format!("zz{SEP}da859a23e6f8c4c3662bdca5b43ee9d5"), KEY32),
            ("암호문 hex 아님", format!("{IV}{SEP}xyz"), KEY32),
            ("홀수 hex", format!("{IV}{SEP}da8"), KEY32),
            ("블록 크기 아님", format!("{IV}{SEP}da859a23"), KEY32),
            ("빈 암호문", format!("{IV}{SEP}"), KEY32),
            ("잘못된 bytea 키", good.clone(), r"bad\key"),
            ("잘못된 hex 키", good.clone(), r"\xzz"),
            ("끝이 잘린 8진", good.clone(), r"ab\10"),
        ];
        for (name, input, key) in cases {
            match decrypt(&input, key) {
                Ok(got) => panic!("{name}: 오류여야 한다, got {got:?}"),
                Err(e) => assert!(!e.contains(key), "{name}: 오류에 키가 있다: {e}"),
            }
        }
    }

    #[test]
    fn 채움은_맞아도_UTF8_이_아니면_오류다() {
        // 0xff 0xfe 뒤에 PKCS#7 채움 14바이트를 붙여 AES-128(키 "k" 를 0 으로 채움)·CBC 로 암호화한다
        let mut key = vec![b'k'];
        key.resize(16, 0);
        let mut data = vec![0xff, 0xfe];
        data.resize(16, 0x0e);
        cbc_encrypt::<aes::Aes128>(&key, &pad_iv(&hex_decode(IV).unwrap()), &mut data);
        let err = decrypt(&format!("{IV}{SEP}{}", hex_encode(&data)), "k").unwrap_err();
        assert!(err.contains("UTF-8"), "UTF-8 아님: {err}");
    }

    #[test]
    fn IV_32자와_블록_단위_hex_만_맞다() {
        let d = "da859a23e6f8c4c3662bdca5b43ee9d5";
        let cases = [
            (format!("{IV}{SEP}{d}"), true),
            (format!("{}{SEP}{}", IV.to_uppercase(), "ab".repeat(32)), true),
            (format!("{IV}{SEP}da859a23"), false),
            (format!("{}{SEP}{d}", &IV[..30]), false),
            (format!("{IV}{SEP}"), false),
            (format!("{IV}^|{d}"), false),
            (format!("{IV}{SEP}da859a23e6f8c4c3662bdca5b43ee9dg"), false),
            (format!(" {IV}{SEP}{d}"), false),
            (format!("{IV}{SEP}{d}{SEP}00000000000000000000000000000000"), false),
            ("010-1234-5678".into(), false),
        ];
        for (input, want) in cases {
            assert_eq!(looks_encrypted(&input), want, "{input:?}");
        }
    }

    // ─── enc_test.go ───

    // 아래 암호문은 위 pgcrypto 값이다: staticEncrypt('010-1234-5678'), 키 KEY32
    fn phone_cipher() -> String {
        format!("{IV}{SEP}da859a23e6f8c4c3662bdca5b43ee9d5")
    }

    fn must(plain: &str) -> String {
        encrypt(plain, KEY32, IV).unwrap()
    }

    #[test]
    fn pgcrypto_와_같은_소문자_hex_를_만든다() {
        assert_eq!(encrypt("010-1234-5678", KEY32, &IV.to_uppercase()), Ok(phone_cipher()));
        // 짧은 IV 는 받은 바이트만 앞에 적고 암호화에는 0 으로 채운 IV 를 쓴다: encrypt_iv('x', 'abc', '\x0102', 'aes')
        assert_eq!(
            encrypt("x", "abc", "0102"),
            Ok(format!("0102{SEP}98072268514e601c107743c2701503ab")),
            "짧은 IV"
        );
    }

    /// Go 는 무작위로 100번 고른다. 여기서는 글·키 조합을 모두 돈다.
    #[test]
    fn Encrypt_Decrypt_로_되돌린다() {
        let x16 = "x".repeat(16);
        let y31 = "y".repeat(31);
        let long = format!("{KEY32}EXTRA");
        let words = ["a@b.com", "홍길동", "😀", "", x16.as_str(), y31.as_str()];
        let keys = ["k", KEY32, long.as_str(), "abcdefghijklmnopqrst", r"a\\b\101", r"\x6b6579"];
        for plain in words {
            for key in keys {
                let enc = encrypt(plain, key, IV).unwrap();
                assert!(looks_encrypted(&enc), "staticEncrypt 꼴이 아니다: {enc:?}");
                assert_eq!(decrypt(&enc, key).as_deref(), Ok(plain), "{key:?}");
            }
        }
    }

    #[test]
    fn 잘못된_IV_와_키는_오류이고_키를_싣지_않는다() {
        let cases = [
            ("IV hex 아님", KEY32, "zz"),
            ("잘못된 bytea 키", r"bad\key", IV),
            ("잘못된 hex 키", r"\xzz", IV),
        ];
        for (name, key, iv) in cases {
            match encrypt("x", key, iv) {
                Ok(got) => panic!("{name}: 오류여야 한다, got {got:?}"),
                Err(e) => assert!(!e.contains(key), "{name}: 오류에 키가 있다: {e}"),
            }
        }
    }

    #[test]
    fn 문자열_주석_밖의_enc_만_암호문_리터럴로_바꾼다() {
        let q = format!("'{}'", phone_cipher());
        let cases = [
            (
                "기본",
                "select * from users where phone = enc('010-1234-5678')".to_string(),
                format!("select * from users where phone = {q}"),
                1,
            ),
            (
                "대소문자·공백",
                "where a = ENC ( '010-1234-5678' ) and b = Enc('010-1234-5678')".into(),
                format!("where a = {q} and b = {q}"),
                2,
            ),
            (
                "'' 이스케이프는 값으로 읽는다",
                "select enc('it''s')".into(),
                format!("select '{}'", must("it's")),
                1,
            ),
            (
                "E'' 이스케이프",
                r"select enc(E'a\nb\x41\101é\'')".into(),
                format!("select '{}'", must("a\nbAAé'")),
                1,
            ),
            ("문자열 안", "select 'enc(''x'')'".into(), "select 'enc(''x'')'".into(), 0),
            ("줄 주석 안", "select 1 -- enc('x')\n".into(), "select 1 -- enc('x')\n".into(), 0),
            (
                "블록 주석 안",
                "select /* enc('x') */ 1".into(),
                "select /* enc('x') */ 1".into(),
                0,
            ),
            (
                "$$ 안",
                "do $$ begin perform enc('x'); end $$".into(),
                "do $$ begin perform enc('x'); end $$".into(),
                0,
            ),
            ("따옴표 이름", r#"select "enc"('x')"#.into(), r#"select "enc"('x')"#.into(), 0),
            ("한정 이름", "select s.enc('x')".into(), "select s.enc('x')".into(), 0),
            (
                "다른 이름의 일부",
                "select myenc('x'), enc_x('y'), encode('z', 'hex')".into(),
                "select myenc('x'), enc_x('y'), encode('z', 'hex')".into(),
                0,
            ),
            (
                "열 이름 enc",
                "select enc from t where enc = 1".into(),
                "select enc from t where enc = 1".into(),
                0,
            ),
        ];
        for (name, input, want, n) in cases {
            assert_eq!(expand_enc(&input, KEY32, IV), Ok((want, n, false)), "{name}");
        }
    }

    #[test]
    fn 키나_IV_가_없으면_서버_staticEncrypt_로_바꾼다() {
        let input = "select * from users where email = enc( 'a@b.com' ) -- enc('x')";
        let want = "select * from users where email = staticEncrypt( 'a@b.com' ) -- enc('x')";
        for (key, iv) in [("", IV), (KEY32, ""), ("", "")] {
            assert_eq!(expand_enc(input, key, iv), Ok((want.to_string(), 1, true)), "{key:?} {iv:?}");
        }
        // 바꿀 enc 가 없으면 server 는 거짓이다
        assert_eq!(expand_enc("select 1", "", ""), Ok(("select 1".to_string(), 0, false)), "enc 없음");
        // 서버 쪽으로 바꿀 때도 인자 꼴은 검사한다
        assert!(
            expand_enc("select enc(col)", "", "").is_err(),
            "서버 바꾸기에서도 리터럴이 아니면 오류여야 한다"
        );
    }

    #[test]
    fn 잘못된_꼴은_위치를_알리고_키와_글을_싣지_않는다() {
        let cases = [
            ("리터럴 아님", "select enc(email)", "1:8"),
            ("빈 인자", "select\n  enc()", "2:3"),
            ("리터럴 둘", "select enc('secret', 'b')", "1:8"),
            ("닫는 괄호 없음", "select 한글, enc('secret'", "1:12"),
            ("닫히지 않은 리터럴", "select enc('secret", "1:8"),
            ("리터럴 뒤 연산", "select enc('secret' || 'x')", "1:8"),
            ("모자란 유니코드 이스케이프", r"select enc(E'secret\u12')", "1:8"),
        ];
        for (name, input, pos) in cases {
            let err = match expand_enc(input, KEY32, IV) {
                Ok(got) => panic!("{name}: 오류여야 한다, got {got:?}"),
                Err(e) => e,
            };
            assert!(err.contains(&format!("at {pos}:")), "{name}: 위치 {pos} 가 없다: {err}");
            assert!(
                !err.contains(KEY32) && !err.contains("secret"),
                "{name}: 오류에 키·글이 있다: {err}"
            );
        }
        let err = expand_enc("select enc('x')", r"bad\key", IV).unwrap_err();
        assert!(!err.contains(r"bad\key"), "잘못된 키: {err}");
    }

    #[test]
    fn 테이블_함수_이름_자리의_enc_는_그대로_둔다() {
        for sql in [
            "insert into enc (a) values (1)",
            "create table enc (a int)",
            "create or replace function enc(t text) returns text as $$ select t $$ language sql",
            "drop procedure enc()",
            "update enc set a = 1",
            "select * from enc ('x')",
            "select * from t join enc ('x') on true",
            "insert INTO /* 주석 */ enc (a) values (1)",
        ] {
            assert_eq!(expand_enc(sql, KEY32, IV), Ok((sql.to_string(), 0, false)), "{sql:?}");
        }
        // 이름 자리 뒤의 호출은 그대로 바꾼다
        assert_eq!(
            expand_enc("insert into enc (a) values (enc('010-1234-5678'))", KEY32, IV),
            Ok((format!("insert into enc (a) values ('{}')", phone_cipher()), 1, false))
        );
    }

    // ─── Go 와 같은 오류 글 ───

    #[test]
    fn 오류_글은_Go_와_같다() {
        assert_eq!(
            expand_enc("select enc(email)", KEY32, IV),
            Err("enc() at 1:8: argument must be a single string literal".into())
        );
        assert_eq!(
            expand_enc(r"select enc(E'\u12')", KEY32, IV),
            Err(r"enc() at 1:8: invalid Unicode escape \u".into())
        );
        assert_eq!(
            expand_enc(r"select enc(E'\UFFFFFFFF')", KEY32, IV),
            Err(r"enc() at 1:8: invalid Unicode escape value \U".into())
        );
        assert_eq!(
            expand_enc("select enc('x')", KEY32, "zz"),
            Err("enc() at 1:8: invalid IV hex".into())
        );
    }

    #[test]
    fn E리터럴의_바이트_이스케이프는_UTF8_이_아니어도_암호화한다() {
        assert_eq!(unquote(r"\xff\777\x\q", true), Ok(vec![0xff, 0xff, b'x', b'q']));
        let (out, n, _) = expand_enc(r"select enc(E'\xff')", KEY32, IV).unwrap();
        assert_eq!(n, 1);
        assert!(looks_encrypted(out.trim_start_matches("select '").trim_end_matches('\'')));
    }
}
