// Package finder 는 접속 전체의 테이블·뷰·함수·Redis 키를 퍼지 검색으로 고르는 창이다.
package finder

import (
	"cmp"
	"slices"
	"unicode"
)

const (
	MaxMatches = 50

	scoreMatch       = 16
	bonusFirst       = 12 // 대상 맨 앞 글자
	bonusBoundary    = 10 // 구분자(. _ - : / 공백) 뒤나 소문자→대문자(camelCase) 첫 글자
	bonusConsecutive = 8
	penaltyGapStart  = 3
	penaltyGapExtend = 1
	maxLengthPenalty = 10 // 길이 차 벌점 상한. 긴 이름이 짧은 이름보다 늘 뒤로 밀리지는 않게 한다
	noScore          = -1 << 30
	// 점수를 매길 때 보는 대상 길이 상한이다. 표(DP)가 대상×질의 크기라 20만 자 이름 하나가 수천만 칸을
	// 잡던 것을 막는다(질의는 model 이 maxQueryRunes 로 자른다). 앞 256 룬 뒤에만 있는 글자로는 찾지 못한다
	maxTargetRunes = 256
)

const (
	KindTable    = "table"
	KindView     = "view"
	KindFunction = "function"
	KindRedisKey = "redis-key"
	// KindColumn 의 Name 은 "테이블.열" 이라 Target 이 "스키마.테이블.열" 이 된다(R56)
	KindColumn = "column"
)

type Item struct{ Conn, Kind, Schema, Name string }

// Hit 는 Match 의 결과 하나다. 계약 문서는 []Match 라 적었지만 Go 는 함수와 타입이 같은 이름을 가질 수 없다.
// Pos 는 Target(Item) 의 룬 위치로, 강조할 글자를 가리킨다.
type Hit struct {
	Item  Item
	Index int // 입력 items 에서의 위치
	Score int
	Pos   []int
}

type scored struct{ idx, score int }

// scratch 는 항목마다 쓰는 버퍼다. 한 번 검색에서 할당을 줄이려고 재사용한다.
type scratch struct {
	target, query []rune // 소문자로 바꾼 글
	orig          []rune // 대소문자 그대로(camelCase 경계 판정용)
	pos, bonus    []int
	dp            []int // 질의 j 행 × 대상 i 열. 그 자리에 맞출 수 없으면 noScore
}

// Target 은 검색·표시 대상 글이다. 스키마가 있으면 "스키마.이름" 이다.
func Target(it Item) string {
	if it.Schema == "" {
		return it.Name
	}
	return it.Schema + "." + it.Name
}

// Match 는 q 의 글자가 순서대로(떨어져도 됨) 들어 있는 항목을 점수 높은 순으로 MaxMatches 개까지 돌려준다.
// 대소문자를 가리지 않고 한글은 음절 단위로 맞춘다. 점수가 같으면 입력 순서를 지킨다.
// 대상은 앞 maxTargetRunes 룬까지만 본다.
// q 가 비면 앞에서부터 MaxMatches 개를 점수 0 으로 돌려준다.
func Match(items []Item, q string) []Hit {
	s := &scratch{}
	for _, r := range q {
		if !unicode.IsSpace(r) {
			s.query = append(s.query, unicode.ToLower(r))
		}
	}
	if len(s.query) == 0 {
		out := make([]Hit, 0, min(MaxMatches, len(items)))
		for i, it := range items[:min(MaxMatches, len(items))] {
			out = append(out, Hit{Item: it, Index: i})
		}
		return out
	}
	var hits []scored
	for i, it := range items {
		if sc, ok := s.score(it); ok {
			hits = append(hits, scored{i, sc})
		}
	}
	slices.SortStableFunc(hits, func(a, b scored) int { return cmp.Compare(b.score, a.score) })
	out := make([]Hit, 0, min(MaxMatches, len(hits)))
	for _, h := range hits[:min(MaxMatches, len(hits))] {
		s.score(items[h.idx]) // 위치는 남은 것만 다시 구해 복사한다
		out = append(out, Hit{Item: items[h.idx], Index: h.idx, Score: h.score, Pos: slices.Clone(s.pos)})
	}
	return out
}

// score 는 s.query 가 it 의 Target 에 순서대로 들어 있으면 가장 좋은 배치의 점수를 내고 s.pos 에 위치를 남긴다.
// 앞에서부터 처음 맞는 글자만 고르면 "ur" 이 user_roles 의 roles 첫 글자 대신 user 의 r 을 잡으므로,
// 질의 글자 j 를 대상 i 에 맞출 때의 최고 점수를 표(DP, 대상 길이×질의 길이)로 셈한다.
func (s *scratch) score(it Item) (int, bool) {
	// []rune(...) 변환은 항목마다 할당하므로 버퍼에 룬을 직접 붙인다
	s.orig = s.orig[:0]
	if it.Schema != "" {
		for _, r := range it.Schema {
			if len(s.orig) == maxTargetRunes {
				break
			}
			s.orig = append(s.orig, r)
		}
		if len(s.orig) < maxTargetRunes {
			s.orig = append(s.orig, '.')
		}
	}
	for _, r := range it.Name {
		if len(s.orig) == maxTargetRunes {
			break
		}
		s.orig = append(s.orig, r)
	}
	t, q := s.target[:0], s.query
	for _, r := range s.orig {
		t = append(t, unicode.ToLower(r))
	}
	s.target = t
	n, m := len(t), len(q)
	// 순서대로 다 들어 있는지 먼저 본다. 대부분 항목은 여기서 빠져 표를 만들지 않는다
	qi := 0
	for _, r := range t {
		if qi < m && r == q[qi] {
			qi++
		}
	}
	if qi < m {
		return 0, false
	}
	s.bonus = s.bonus[:0]
	for i := range n {
		// 낱말 첫 글자: 맨 앞, 구분자 뒤, 소문자 뒤 대문자(camelCase), 숫자·글자가 바뀌는 자리
		b := 0
		switch {
		case i == 0:
			b = bonusFirst
		case !unicode.IsLetter(s.orig[i-1]) && !unicode.IsDigit(s.orig[i-1]),
			unicode.IsLower(s.orig[i-1]) && unicode.IsUpper(s.orig[i]),
			unicode.IsDigit(s.orig[i-1]) != unicode.IsDigit(s.orig[i]):
			b = bonusBoundary
		}
		s.bonus = append(s.bonus, b)
	}
	if cap(s.dp) < n*m {
		s.dp = make([]int, n*m)
	}
	dp := s.dp[:n*m]
	for j := range m {
		row := dp[j*n : (j+1)*n]
		// gap 은 앞 글자를 i-2 이하에 맞추고 사이를 띄운 경우의 최고 점수다. 한 칸 갈 때마다 연장 벌점을 뺀다
		gap := noScore
		for i := range n {
			from := 0 // 첫 질의 글자는 앞이 비어도 벌점이 없다
			if j > 0 {
				prev := dp[(j-1)*n : j*n]
				if i >= 2 {
					gap = max(gap-penaltyGapExtend, prev[i-2]-penaltyGapStart)
				}
				from = gap
				if i >= 1 && prev[i-1] > noScore/2 {
					from = max(from, prev[i-1]+bonusConsecutive)
				}
			}
			if t[i] != q[j] || from <= noScore/2 {
				row[i] = noScore
				continue
			}
			row[i] = from + scoreMatch + s.bonus[i]
		}
	}
	last := dp[(m-1)*n:]
	end := 0
	for i := range n {
		if last[i] > last[end] {
			end = i
		}
	}
	// 표를 거꾸로 따라가 각 질의 글자의 위치를 되찾는다
	s.pos = append(s.pos[:0], make([]int, m)...)
	for j, i := m-1, end; j >= 0; j-- {
		s.pos[j] = i
		if j == 0 {
			break
		}
		v := dp[j*n+i] - scoreMatch - s.bonus[i]
		prev := dp[(j-1)*n : j*n]
		if i >= 1 && prev[i-1] > noScore/2 && prev[i-1]+bonusConsecutive == v {
			i--
			continue
		}
		for k := i - 2; k >= 0; k-- {
			if prev[k] > noScore/2 && prev[k]-penaltyGapStart-penaltyGapExtend*(i-k-2) == v {
				i = k
				break
			}
		}
	}
	return last[end] - min(maxLengthPenalty, (n-m)/4), true
}
