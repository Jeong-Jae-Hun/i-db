package db

import (
	"slices"
	"strings"
)

// 결과 표는 NULL 을 "NULL" 글자로 싣는다. 진짜 'NULL' 문자열과 구별할 수 없어 복사본에서는 NULL 로 본다.
const (
	unknownOriginComment = "-- 원본 테이블을 알 수 없음\n"
	truncatedComment     = "-- 잘린 값이 있어 원본과 다를 수 있음\n"
)

// InsertSQL 은 고른 행을 INSERT 한 문장으로 만든다. 식 열·생성 열·GENERATED ALWAYS IDENTITY 열은 뺀다.
// 출처를 모르면 모든 열을 "table" 에 넣는 틀을 주고 그 사실을 주석으로 알린다.
func InsertSQL(r *Result, rows []int) string {
	head := ""
	table := QuoteIdent("table")
	var skip []string
	if r.Origin != nil {
		table, skip = r.Origin.Table.Quoted(), r.Origin.Always
	} else {
		head = unknownOriginComment
	}
	idx, names := copyColumns(r, skip)
	var b strings.Builder
	truncated := false
	for _, ri := range rows {
		if ri < 0 || ri >= len(r.Rows) {
			continue
		}
		if b.Len() > 0 {
			b.WriteString(",\n")
		}
		b.WriteString("  (")
		for j, ci := range idx {
			if j > 0 {
				b.WriteString(", ")
			}
			b.WriteString(cellLiteral(r.Rows[ri][ci]))
			truncated = truncated || len(r.Rows[ri][ci]) > MaxCellBytes
		}
		b.WriteString(")")
	}
	if b.Len() == 0 || len(idx) == 0 {
		return ""
	}
	if truncated {
		head += truncatedComment
	}
	return head + "INSERT INTO " + table + " (" + strings.Join(names, ", ") + ") VALUES\n" + b.String() + ";"
}

// UpdateSQL 은 고른 행마다 PK 로 찾아 PK 가 아닌 열을 모두 덮는 UPDATE 를 만든다. 출처를 모르면 만들 수 없다.
func UpdateSQL(r *Result, rows []int) string {
	if r.Origin == nil {
		return unknownOriginComment
	}
	// ALWAYS IDENTITY 는 PK 라 SET 에 들어가지 않고 WHERE 로만 쓰인다
	idx, names := copyColumns(r, nil)
	var set, where []int
	for j, ci := range idx {
		if slices.Contains(r.Origin.PK, r.Origin.Cols[ci]) {
			where = append(where, j)
		} else {
			set = append(set, j)
		}
	}
	if len(set) == 0 {
		return "-- 기본 키가 아닌 열이 없어 UPDATE 할 것이 없음\n"
	}
	var b strings.Builder
	truncated := false
	for _, ri := range rows {
		if ri < 0 || ri >= len(r.Rows) {
			continue
		}
		row := r.Rows[ri]
		b.WriteString("UPDATE " + r.Origin.Table.Quoted() + " SET ")
		for k, j := range set {
			if k > 0 {
				b.WriteString(", ")
			}
			b.WriteString(names[j] + " = " + cellLiteral(row[idx[j]]))
			truncated = truncated || len(row[idx[j]]) > MaxCellBytes
		}
		for k, j := range where {
			if k == 0 {
				b.WriteString(" WHERE ")
			} else {
				b.WriteString(" AND ")
			}
			b.WriteString(names[j] + " = " + cellLiteral(row[idx[j]]))
		}
		b.WriteString(";\n")
	}
	if truncated {
		return truncatedComment + b.String()
	}
	return b.String()
}

// copyColumns 는 복사할 열의 결과 위치와 따옴표 친 테이블 열 이름이다. 출처가 있으면 식 열과 겹친 열, skip 의 열을 뺀다.
func copyColumns(r *Result, skip []string) (idx []int, names []string) {
	if r.Origin == nil {
		for i, c := range r.Cols {
			idx, names = append(idx, i), append(names, QuoteIdent(c))
		}
		return idx, names
	}
	seen := map[string]bool{}
	for i, c := range r.Origin.Cols {
		if c == "" || seen[c] || slices.Contains(skip, c) {
			continue
		}
		seen[c] = true
		idx, names = append(idx, i), append(names, QuoteIdent(c))
	}
	return idx, names
}

func cellLiteral(v string) string {
	if v == "NULL" {
		return "NULL"
	}
	return QuoteLiteral(v)
}
