package main

import (
	"encoding/csv"
	"errors"
	"fmt"
	"io"
	"os"
	"slices"
	"strings"
)

// ─── CSV 가져오기 ───

const csvPreviewRows = 5

// csvPlan 은 CSV 를 테이블 열에 맞춘 결과다. 이름이 맞지 않는 CSV 열은 버린다.
type csvPlan struct {
	columns []string    // 넣을 테이블 열(CSV 순서)
	rows    [][]*string // columns 순서의 값. 빈 칸은 NULL
	dropped []string    // 맞는 테이블 열이 없어 버린 CSV 머리
	preview []string
}

// readCSVPlan 은 머리 줄이 있는 CSV 를 읽어 이름이 같은 테이블 열(대소문자 무시)에 맞춘다.
// 빈 칸은 NULL 로 넣는다. encoding/csv 가 따옴표 친 빈 값과 빈 칸을 가르지 않아서다.
func readCSVPlan(path string, cols []string) (csvPlan, error) {
	f, err := os.Open(path)
	if err != nil {
		return csvPlan{}, err
	}
	defer f.Close()
	r := csv.NewReader(f)
	head, err := r.Read()
	if errors.Is(err, io.EOF) {
		return csvPlan{}, errors.New("empty csv")
	}
	if err != nil {
		return csvPlan{}, err
	}
	var p csvPlan
	var idx []int
	lower := make([]string, len(cols))
	for i, c := range cols {
		lower[i] = strings.ToLower(c)
	}
	for i, h := range head {
		h = strings.TrimPrefix(strings.TrimSpace(h), "\ufeff") // 엑셀이 붙이는 BOM
		j := slices.Index(cols, h)
		if j < 0 {
			j = slices.Index(lower, strings.ToLower(h))
		}
		if j < 0 || slices.Contains(p.columns, cols[j]) {
			p.dropped = append(p.dropped, h)
			p.preview = append(p.preview, fmt.Sprintf("  %s → (버림)", h))
			continue
		}
		idx = append(idx, i)
		p.columns = append(p.columns, cols[j])
		p.preview = append(p.preview, fmt.Sprintf("  %s → %s", h, cols[j]))
	}
	if len(p.columns) == 0 {
		return csvPlan{}, errors.New("no csv header matches a table column")
	}
	for {
		rec, err := r.Read()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return csvPlan{}, err
		}
		row := make([]*string, len(idx))
		for k, i := range idx {
			if i < len(rec) && rec[i] != "" {
				v := rec[i]
				row[k] = &v
			}
		}
		p.rows = append(p.rows, row)
	}
	p.preview = append([]string{fmt.Sprintf("CSV 열 → 테이블 열 (%d행, 빈 칸은 NULL, 한 문장 COPY 라 실패하면 한 행도 남지 않음)", len(p.rows))}, p.preview...)
	p.preview = append(p.preview, "", "앞 "+fmt.Sprint(min(csvPreviewRows, len(p.rows)))+"행: "+strings.Join(p.columns, " | "))
	for _, row := range p.rows[:min(csvPreviewRows, len(p.rows))] {
		vals := make([]string, len(row))
		for i, v := range row {
			vals[i] = "NULL"
			if v != nil {
				vals[i] = *v
			}
		}
		p.preview = append(p.preview, "  "+strings.Join(vals, " | "))
	}
	return p, nil
}
