// Package resultdiff 는 두 결과 표를 비교하고(R31.2) 고른 칸을 집계한다(R31.1).
package resultdiff

import (
	"errors"
	"fmt"
	"math/big"
	"slices"
	"strconv"
	"strings"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

const (
	nullText = "NULL" // db.collectRows 가 NULL 을 이 글자로 싣는다
	// 나누어떨어지지 않는 평균(1/3 등)만 반올림한다. 1 이상이면 소수 이 자리, 1 미만이면 유효 숫자 이 자리다.
	// 합·최소·최대는 늘 정확한 값이다
	avgPlaces = 10
)

// ─── 결과 비교 ───

type Op int

const (
	Added Op = iota + 1
	Removed
	Changed
)

// Row 는 달라진 행 하나다. Row 는 b 의 행(삭제면 a 의 행)이고 입력 결과의 슬라이스를 그대로 가리킨다.
// ChangedCols 는 바뀜일 때 값이 다른 열 번호다.
type Row struct {
	Op          Op
	Row         []string
	ChangedCols []int
}

// Report 의 Rows 는 b 의 순서를 따르고, 삭제된 행은 끝에 a 의 순서로 붙는다. 같은 행은 넣지 않는다.
type Report struct {
	Added, Removed, Changed int
	Rows                    []Row
}

// Diff 는 a(고정한 결과)에서 b(지금 결과)로 무엇이 바뀌었는지 돌려준다. 열 이름이 순서까지 같아야 한다.
// 두 결과가 같은 테이블에서 왔고 PK 가 같으면 PK 로 짝을 지어 바뀐 칸을 찾는다. 아니면 행 전체 값으로 짝을 짓고
// 같은 행이 여럿이면 개수로 맞춘다(a 에 둘, b 에 셋이면 추가 하나). 이때는 바뀜이 없고 추가·삭제만 나온다.
// 출처가 다른 두 테이블은 PK 이름이 같아도 같은 행을 가리키지 않으므로 Table 까지 같아야 PK 로 짓는다.
// NULL 은 "NULL" 글자로 실려 오므로 문자열 'NULL' 과 같게 본다.
func Diff(a, b *db.Result) (Report, error) {
	if a == nil || b == nil || len(a.Cols) == 0 || len(b.Cols) == 0 {
		return Report{}, errors.New("표가 아닌 결과는 비교할 수 없음")
	}
	if !slices.Equal(a.Cols, b.Cols) {
		return Report{}, fmt.Errorf("열이 달라 비교할 수 없음: %s", colsDiff(a.Cols, b.Cols))
	}
	keyA, keyB := pkIndexes(a, b)
	pending := make(map[string][]int, len(a.Rows)) // 키 → 아직 짝이 없는 a 행 번호(a 순서)
	for i, row := range a.Rows {
		k := rowKey(row, keyA)
		pending[k] = append(pending[k], i)
	}
	matched := make([]bool, len(a.Rows))
	var r Report
	for _, row := range b.Rows {
		k := rowKey(row, keyB)
		idx := pending[k]
		if len(idx) == 0 {
			r.Added++
			r.Rows = append(r.Rows, Row{Op: Added, Row: row})
			continue
		}
		pending[k] = idx[1:]
		matched[idx[0]] = true
		old := a.Rows[idx[0]]
		var changed []int
		for c := range b.Cols {
			if cell(old, c) != cell(row, c) {
				changed = append(changed, c)
			}
		}
		if len(changed) > 0 {
			r.Changed++
			r.Rows = append(r.Rows, Row{Op: Changed, Row: row, ChangedCols: changed})
		}
	}
	for i, row := range a.Rows {
		if !matched[i] {
			r.Removed++
			r.Rows = append(r.Rows, Row{Op: Removed, Row: row})
		}
	}
	return r, nil
}

// Summary 는 표 위에 적을 `+N −N ~N` 이다.
func (r Report) Summary() string {
	return fmt.Sprintf("+%d −%d ~%d", r.Added, r.Removed, r.Changed)
}

// pkIndexes 는 PK 로 짝지을 수 있으면 두 결과 각각의 PK 열 번호를, 아니면 둘 다 nil(행 전체)을 돌려준다.
// Origin.Cols 는 결과 열마다의 테이블 열 이름이라 결과 열 별칭이 달라도 찾는다.
func pkIndexes(a, b *db.Result) ([]int, []int) {
	if a.Origin == nil || b.Origin == nil || a.Origin.Table != b.Origin.Table || !slices.Equal(a.Origin.PK, b.Origin.PK) {
		return nil, nil
	}
	var ia, ib []int
	for _, pk := range a.Origin.PK {
		i, j := slices.Index(a.Origin.Cols, pk), slices.Index(b.Origin.Cols, pk)
		if i < 0 || j < 0 {
			return nil, nil
		}
		ia, ib = append(ia, i), append(ib, j)
	}
	return ia, ib
}

// rowKey 는 idx 열(nil 이면 전체)의 값을 길이를 앞에 붙여 잇는다. 구분자만 쓰면 값 안의 구분자로 다른 행이 같은 키가 된다.
func rowKey(row []string, idx []int) string {
	n := len(idx)
	if idx == nil {
		n = len(row)
	}
	var sb strings.Builder
	for k := range n {
		v := cell(row, k)
		if idx != nil {
			v = cell(row, idx[k])
		}
		sb.WriteString(strconv.Itoa(len(v)))
		sb.WriteByte(':')
		sb.WriteString(v)
	}
	return sb.String()
}

// cell 은 짧은 행(Redis 등 열 수가 고르지 않은 결과)의 빈 칸을 빈 글로 읽는다.
func cell(row []string, i int) string {
	if i < len(row) {
		return row[i]
	}
	return ""
}

// colsDiff 는 a 에만 있는 열을 -, b 에만 있는 열을 + 로 적는다. 같은 열이 순서만 다르면 그렇다고 적는다.
func colsDiff(a, b []string) string {
	count := map[string]int{}
	for _, c := range b {
		count[c]++
	}
	var parts []string
	for _, c := range a {
		if count[c] > 0 {
			count[c]--
			continue
		}
		parts = append(parts, "-"+c)
	}
	for _, c := range b {
		if count[c] > 0 {
			count[c]--
			parts = append(parts, "+"+c)
		}
	}
	if len(parts) == 0 {
		return "열 순서가 다름"
	}
	return strings.Join(parts, " ")
}

// ─── 선택 칸 집계 ───

// Stats 는 고른 칸의 집계다. Cells 는 모든 칸, Count 는 그중 숫자로 읽힌 칸 수이고 Sum·Min·Max 는 Count 가 0 이면 nil 이다.
// numeric 은 bigint·numeric(1000 자리까지)을 float64 로 읽으면 15자리 뒤를 잃으므로 big.Rat 으로 정확히 센다.
type Stats struct {
	Cells, Count  int
	Sum, Min, Max *big.Rat
}

// Aggregate 는 cells[i] 를 numeric[i] 가 참일 때만 숫자로 센다. numeric 은 칸의 열이 숫자 열인지다(grid 의 열 종류).
// NULL·NaN·Infinity·숫자로 읽히지 않는 값은 칸 수에만 든다.
func Aggregate(cells []string, numeric []bool) Stats {
	s := Stats{Cells: len(cells)}
	for i, v := range cells {
		// big.Rat 은 "1/3" 같은 분수도 읽지만 DB 가 숫자 열에 그렇게 싣지 않으므로 숫자가 아닌 글로 본다
		if i >= len(numeric) || !numeric[i] || v == nullText || strings.ContainsRune(v, '/') {
			continue
		}
		x, ok := new(big.Rat).SetString(strings.TrimSpace(v))
		if !ok {
			continue
		}
		if s.Count == 0 {
			s.Sum, s.Min, s.Max = new(big.Rat), x, x
		}
		s.Count++
		s.Sum.Add(s.Sum, x)
		if x.Cmp(s.Min) < 0 {
			s.Min = x
		}
		if x.Cmp(s.Max) > 0 {
			s.Max = x
		}
	}
	return s
}

// String 은 상태 줄 글이다: `칸 N · 합 X · 평균 Y · 최소 A · 최대 B`, 숫자 칸이 없으면 `칸 N`.
func (s Stats) String() string {
	if s.Count == 0 {
		return fmt.Sprintf("칸 %d", s.Cells)
	}
	avg := new(big.Rat).Quo(s.Sum, new(big.Rat).SetInt64(int64(s.Count)))
	return fmt.Sprintf("칸 %d · 합 %s · 평균 %s · 최소 %s · 최대 %s",
		s.Cells, formatRat(s.Sum, -1), formatRat(avg, avgPlaces), formatRat(s.Min, -1), formatRat(s.Max, -1))
}

// formatRat 은 r 을 지수 없는 십진수로 적는다. 십진 입력의 합·최소·최대는 분모가 2·5 만으로 되어 늘 끝나는 소수라
// 필요한 자리만큼 정확히 적는다. 끝나지 않거나 limit(0 이상)보다 긴 소수는 반올림하고 끝 0 을 뗀다. 반올림 자리는
// 소수점 뒤 첫 유효 숫자 앞의 0 개수에 limit 을 더한 것이라, 1e-12 같은 0 이 아닌 작은 값이 0 으로 적히지 않는다.
// limit 이 음수면 끝나지 않는 소수도 avgPlaces 자리로 적는다(십진 입력에서는 일어나지 않는다).
func formatRat(r *big.Rat, limit int) string {
	places, exact := decimalPlaces(r.Denom())
	if limit >= 0 {
		limit += leadingZeros(r)
	}
	switch {
	case !exact && limit < 0:
		places = avgPlaces
	case limit >= 0 && (!exact || places > limit):
		places = limit
	}
	s := r.FloatString(places)
	if strings.Contains(s, ".") {
		s = strings.TrimRight(strings.TrimRight(s, "0"), ".")
	}
	if s == "-0" {
		return "0"
	}
	return s
}

// leadingZeros 는 0 < |r| < 1 일 때 소수점 뒤 첫 유효 숫자 앞의 0 개수(|r|·10^k < 1 인 가장 큰 k)이고, 아니면 0 이다.
func leadingZeros(r *big.Rat) int {
	a := new(big.Rat).Abs(r)
	one := big.NewRat(1, 1)
	if a.Sign() == 0 || a.Cmp(one) >= 0 {
		return 0
	}
	// 분자가 n 자리, 분모가 d 자리면 답은 d-n-1 또는 d-n 이다. 10^(d-n) 을 곱해 1 미만인지로 가른다
	k := len(a.Denom().String()) - len(a.Num().String()) - 1
	pow := new(big.Int).Exp(big.NewInt(10), big.NewInt(int64(k+1)), nil)
	if new(big.Rat).Mul(a, new(big.Rat).SetInt(pow)).Cmp(one) < 0 {
		k++
	}
	return k
}

// decimalPlaces 는 1/d 가 끝나는 소수면 그 자릿수를 돌려준다. d = 2^a·5^b 일 때만 끝나고 자릿수는 max(a, b) 다.
func decimalPlaces(d *big.Int) (int, bool) {
	two, five := big.NewInt(2), big.NewInt(5)
	n := new(big.Int).Set(d)
	rem := new(big.Int)
	var a, b int
	for {
		q, m := new(big.Int).QuoRem(n, two, rem)
		if m.Sign() != 0 {
			break
		}
		n, a = q, a+1
	}
	for {
		q, m := new(big.Int).QuoRem(n, five, rem)
		if m.Sign() != 0 {
			break
		}
		n, b = q, b+1
	}
	return max(a, b), n.Cmp(big.NewInt(1)) == 0
}
