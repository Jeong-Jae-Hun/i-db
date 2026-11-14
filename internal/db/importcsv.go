package db

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"strings"
)

// 가져오기는 메모리에 CSV 를 다 만든 뒤 보낸다. 상한은 그 버퍼 크기를 막는다.
const (
	maxImportRows  = 100_000
	maxImportBytes = 50 << 20
)

type ImportSpec struct {
	Table   TableRef
	Columns []string
	Rows    [][]*string // nil 은 NULL
}

// ImportCSV 는 행을 COPY 한 번으로 넣는다. COPY 는 문장 하나라 중간에 실패하면 한 행도 남지 않는다.
// 값은 텍스트로 보내 서버가 열 타입의 입력 규칙으로 읽는다. 편집기 세션의 트랜잭션과 섞이지 않게 풀 연결을 쓴다.
func (p *PGConn) ImportCSV(ctx context.Context, spec ImportSpec) (int64, error) {
	if len(spec.Columns) == 0 {
		return 0, errors.New("import needs at least one column")
	}
	if len(spec.Rows) > maxImportRows {
		return 0, fmt.Errorf("import has %d rows, limit is %d", len(spec.Rows), maxImportRows)
	}
	// CSV 형식에서 따옴표 없는 빈 칸만 NULL 이다. 값은 늘 따옴표로 감싸 빈 문자열·"\N" 같은 글자가 NULL 이 되지 않게 한다
	var buf bytes.Buffer
	// 행마다 폭을 검사하고 버퍼가 상한을 넘는 즉시 멈추려고 한 행씩 쓴다
	for i, row := range spec.Rows {
		if len(row) != len(spec.Columns) {
			return 0, fmt.Errorf("row %d has %d values, expected %d", i+1, len(row), len(spec.Columns))
		}
		for j, v := range row {
			if j > 0 {
				buf.WriteByte(',')
			}
			if v != nil {
				buf.WriteString(QuoteIdent(*v)) // CSV 따옴표 규칙(" 를 "" 로)이 식별자 따옴표 규칙과 같다
			}
		}
		buf.WriteByte('\n')
		if buf.Len() > maxImportBytes {
			return 0, fmt.Errorf("import data exceeds %d MiB", maxImportBytes>>20)
		}
	}
	cols := make([]string, len(spec.Columns))
	// CSV 값 순서와 맞도록 열 순서를 지켜 따옴표만 씌운다
	for i, c := range spec.Columns {
		cols[i] = QuoteIdent(c)
	}
	c, err := p.pool.Acquire(ctx)
	if err != nil {
		return 0, err
	}
	defer c.Release()
	tag, err := c.Conn().PgConn().CopyFrom(ctx, &buf,
		"COPY "+spec.Table.Quoted()+" ("+strings.Join(cols, ", ")+") FROM STDIN (FORMAT csv)")
	if err != nil {
		// COPY 는 자동 커밋이라 응답을 받기 전에 끊기면 이미 들어갔을 수 있다
		return 0, outcomeUnknown("import", err)
	}
	return tag.RowsAffected(), nil
}
