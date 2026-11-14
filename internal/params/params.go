// Package params 는 SQL 의 :name·$n 자리표시를 찾아 PostgreSQL 바인딩 형식($n)으로 맞춘다.
package params

import (
	"strconv"
	"strings"

	"github.com/Jeong-Jae-Hun/i-db/internal/sqlfmt"
)

// PostgreSQL 이 한 문장에 받는 바인딩 인자의 상한이다. 넘거나 int 로 못 읽는 $n 은 이름 목록을 만들지 않는다
const maxParams = 65535

// Bind 는 :name 을 $k 로 바꾼 SQL 과, $k 마다 물어볼 이름(names[k-1])을 돌려준다.
// $1·$2 처럼 이미 번호인 자리는 그대로 두고 이름을 "1"·"2" 로 쓴다. 같은 :name 은 같은 번호를 받고,
// 번호와 섞이면 :name 은 가장 큰 $n 다음 번호부터 받는다. 문자열·따옴표 이름·주석·$$ 안과
// ::type 캐스트, a[1:2]·a[lo:hi]·a[:n] 처럼 ':' 앞뒤가 붙은 배열 조각은 건드리지 않는다.
func Bind(sql string) (rewritten string, names []string) {
	toks := sqlfmt.Tokenize(sql)
	highest := 0
	for _, t := range toks {
		if t.Kind != sqlfmt.Param {
			continue
		}
		if n, err := strconv.Atoi(t.Text[1:]); err == nil && n <= maxParams {
			highest = max(highest, n)
		}
	}
	for n := 1; n <= highest; n++ {
		names = append(names, strconv.Itoa(n))
	}

	var b strings.Builder
	last := 0
	nums := map[string]int{}
	for i := 0; i+1 < len(toks); i++ {
		colon, word := toks[i], toks[i+1]
		if colon.Kind != sqlfmt.Punct || colon.Text != ":" || colon.End != word.Start {
			continue
		}
		if word.Text == ":" {
			i++ // :: 캐스트 뒤 타입 이름은 자리표시가 아니다
			continue
		}
		// ':' 바로 앞에 이름·숫자가 붙으면 a[lo:hi] 같은 배열 조각이다. 바꾸면 x$1·$$1 처럼 앞 토큰과 붙어
		// 다른 낱말이나 $$ 인용이 되기도 한다. a[:n] 처럼 '[' 바로 뒤도 아래쪽을 생략한 배열 조각이다
		glued := false
		if i > 0 && toks[i-1].End == colon.Start {
			switch toks[i-1].Kind {
			case sqlfmt.Word, sqlfmt.Number, sqlfmt.Quoted, sqlfmt.Param:
				glued = true
			default:
				glued = toks[i-1].Text == "$" || toks[i-1].Text == "["
			}
		}
		if word.Kind != sqlfmt.Word || glued {
			continue
		}
		k, ok := nums[word.Text]
		if !ok {
			names = append(names, word.Text)
			k = len(names)
			nums[word.Text] = k
		}
		b.WriteString(sql[last:colon.Start])
		b.WriteString("$" + strconv.Itoa(k))
		last = word.End
	}
	if last == 0 {
		return sql, names
	}
	b.WriteString(sql[last:])
	return b.String(), names
}
