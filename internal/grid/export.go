package grid

import (
	"bytes"
	"encoding/json"
	"strconv"
	"strings"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

// PostgreSQL COPY 텍스트 형식과 같은 이스케이프다. 칸 안의 탭·줄바꿈이 TSV 칸·행 경계로 읽히지 않게 한다
var tsvEscaper = strings.NewReplacer(`\`, `\\`, "\t", `\t`, "\n", `\n`, "\r", `\r`)

// ToCSV 는 결과를 CSV(RFC 4180, 줄 끝 LF)로 만든다. NULL 은 따옴표 없는 빈 칸, 빈 문자열은 "" 로 써서 둘을 가른다.
// encoding/csv 는 빈 문자열을 따옴표로 감싸지 않아 NULL 과 구별할 수 없어 직접 쓴다.
// 단, db.Result 는 NULL 을 "NULL" 글자로 싣기 때문에 값이 정말 "NULL" 인 문자열도 NULL 로 나간다.
// 열이 하나면 빈 줄이 레코드로 읽히지 않으므로(RFC 4180) NULL 도 "" 로 써서 빈 문자열과 같아진다.
func ToCSV(r *db.Result) string {
	var b strings.Builder
	b.Grow(resultBytes(r))
	for i, c := range r.Cols {
		if i > 0 {
			b.WriteByte(',')
		}
		b.WriteString(csvField(c))
	}
	b.WriteByte('\n')
	for _, row := range r.Rows {
		for i := range r.Cols {
			if i > 0 {
				b.WriteByte(',')
			}
			switch v := cellOf(row, i); {
			case v != nullText:
				b.WriteString(csvField(v))
			case len(r.Cols) == 1:
				b.WriteString(`""`)
			}
		}
		b.WriteByte('\n')
	}
	return b.String()
}

// ToJSON 은 결과를 객체 배열로 만든다. 열 순서를 지키고 같은 이름 열이 덮이지 않게 map 대신 직접 조립한다.
// 숫자·bool·json 타입은 JSON 값 그대로, 나머지는 문자열로 쓴다. NULL 은 null 이다(ToCSV 의 "NULL" 한계와 같다).
// kinds 는 거르기 전 전체 결과로 정한 열 종류다. 거른 행으로 다시 추정하면 같은 열이 문자열·숫자를 오간다.
func ToJSON(r *db.Result, kinds []kind) string {
	var b bytes.Buffer
	b.Grow(resultBytes(r))
	b.WriteByte('[')
	for ri, row := range r.Rows {
		if ri > 0 {
			b.WriteByte(',')
		}
		b.WriteString("\n  {")
		for i, c := range r.Cols {
			if i > 0 {
				b.WriteString(", ")
			}
			b.Write(jsonString(c))
			b.WriteString(": ")
			v := cellOf(row, i)
			switch {
			case v == nullText:
				b.WriteString("null")
			case kinds[i] == kindNumber && json.Valid([]byte(v)):
				b.WriteString(v)
			case kinds[i] == kindBool && (v == "t" || v == "f"):
				b.WriteString(display(kindBool, v))
			case typed(r) && jsonTypes[r.Types[i]] && json.Valid([]byte(v)):
				// Valid 를 통과했으므로 Compact 는 실패하지 않는다
				_ = json.Compact(&b, []byte(v))
			default:
				b.Write(jsonString(v))
			}
		}
		b.WriteByte('}')
	}
	if len(r.Rows) > 0 {
		b.WriteByte('\n')
	}
	b.WriteString("]\n")
	return b.String()
}

// resultBytes 는 내보낼 바이트 수의 하한이다. 버퍼를 미리 잡아 수십 MiB 결과에서 거듭 늘리지 않게 한다.
func resultBytes(r *db.Result) int {
	n := 0
	for _, row := range r.Rows {
		n += len(row) + 1
		for _, v := range row {
			n += len(v)
		}
	}
	return n
}

// csvField 는 칸 하나를 CSV 로 쓴다. = + - @ 탭·CR 로 시작하는 글은 스프레드시트가 수식으로 실행하므로
// 앞에 ' 를 붙인다(CSV 수식 주입). 음수·부호 붙은 숫자는 수식이 아니고 값을 바꾸면 안 되므로 그대로 둔다.
func csvField(s string) string {
	if s != "" && strings.ContainsRune("=+-@\t\r", rune(s[0])) {
		if _, err := strconv.ParseFloat(s, 64); err != nil {
			s = "'" + s
		}
	}
	if s != "" && !strings.ContainsAny(s, ",\"\r\n") && strings.TrimSpace(s) == s {
		return s
	}
	return `"` + strings.ReplaceAll(s, `"`, `""`) + `"`
}

// jsonString 은 문자열을 JSON 문자열로 쓴다. 파일로 내보내는 값이라 HTML 이스케이프(<→\u003c)는 끈다.
// string 의 Encode 는 실패하지 않는다(잘못된 UTF-8 은 U+FFFD 로 바뀐다).
func jsonString(s string) []byte {
	var buf bytes.Buffer
	enc := json.NewEncoder(&buf)
	enc.SetEscapeHTML(false)
	_ = enc.Encode(s)
	return bytes.TrimSuffix(buf.Bytes(), []byte("\n"))
}

// RowTSV 는 행 하나를 탭으로 이은 한 줄로 만든다(헤더 없음).
func RowTSV(row []string) string {
	out := make([]string, len(row))
	for i, v := range row {
		out[i] = tsvEscaper.Replace(v)
	}
	return strings.Join(out, "\t")
}
