package obs

import (
	"cmp"
	"encoding/json"
	"fmt"
	"maps"
	"math"
	"slices"
	"strconv"
	"strings"
	"time"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

// timeLayout 은 결과 time 열 형식이다. chart(R28)의 "2006-01-02 15:04:05.999999999" 로 읽힌다
const timeLayout = "2006-01-02 15:04:05.000"

func kstTime(t time.Time) string { return t.In(textutil.KST).Format(timeLayout) }

// queryData 는 Loki·Prometheus query·query_range 의 data 다.
type queryData struct {
	ResultType string          `json:"resultType"`
	Result     json.RawMessage `json:"result"`
}

type labeledRow struct {
	at     time.Time
	labels map[string]string
	last   string // line 또는 value
}

// toResult 는 resultType 에 맞춰 표로 바꾼다. 로그는 time·라벨·line(최신 먼저),
// 지표는 time·라벨·value(시계열끼리 묶고 그 안은 시간 순)이다. limit 을 넘는 행은 자르고 Truncated 를 켠다.
// asc 면 로그 줄을 오래된 것부터 둔다(Loki forward, R62.3). 지표는 늘 시계열별 서버 순서다.
func toResult(d queryData, limit int, asc bool) (*db.Result, error) {
	var (
		rows []labeledRow
		last = "value"
		err  error
	)
	switch d.ResultType {
	case "streams":
		last = "line"
		rows, err = streamRows(d.Result, asc)
	case "matrix", "vector":
		rows, err = seriesRows(d.Result)
	case "scalar", "string":
		var s sample
		if err = json.Unmarshal(d.Result, &s); err == nil {
			rows = []labeledRow{{at: s.at, last: s.val}}
		}
	default:
		return nil, fmt.Errorf("unsupported result type %q", d.ResultType)
	}
	if err != nil {
		return nil, fmt.Errorf("unexpected %s result: %w", d.ResultType, err)
	}
	return table(rows, last, limit), nil
}

func table(rows []labeledRow, last string, limit int) *db.Result {
	names := map[string]bool{}
	for _, r := range rows {
		for k := range r.labels {
			names[k] = true
		}
	}
	labels := slices.Sorted(maps.Keys(names))
	res := &db.Result{Cols: append(append([]string{"time"}, labels...), last), Rows: [][]string{}}
	if len(rows) > limit {
		rows, res.Truncated = rows[:limit], true
	}
	for _, r := range rows {
		row := make([]string, 0, len(res.Cols))
		row = append(row, kstTime(r.at))
		for _, l := range labels {
			row = append(row, db.CapCell(r.labels[l]))
		}
		res.Rows = append(res.Rows, append(row, db.CapCell(r.last)))
	}
	return res
}

func streamRows(raw json.RawMessage, asc bool) ([]labeledRow, error) {
	var streams []struct {
		Stream map[string]string   `json:"stream"`
		Values [][]json.RawMessage `json:"values"` // [ns 문자열, 줄, (구조화 메타데이터)]
	}
	if err := json.Unmarshal(raw, &streams); err != nil {
		return nil, err
	}
	var rows []labeledRow
	for _, s := range streams {
		for _, v := range s.Values {
			if len(v) < 2 {
				return nil, fmt.Errorf("log entry has %d fields", len(v))
			}
			var ns, line string
			if err := json.Unmarshal(v[0], &ns); err != nil {
				return nil, err
			}
			if err := json.Unmarshal(v[1], &line); err != nil {
				return nil, err
			}
			n, err := strconv.ParseInt(ns, 10, 64)
			if err != nil {
				return nil, fmt.Errorf("bad timestamp %q", ns)
			}
			rows = append(rows, labeledRow{at: time.Unix(0, n), labels: s.Stream, last: line})
		}
	}
	// 스트림마다 따로 오므로 합친 뒤 최신 먼저(asc 면 오래된 것 먼저)로 다시 줄 세운다
	slices.SortStableFunc(rows, func(a, b labeledRow) int {
		if asc {
			return a.at.Compare(b.at)
		}
		return b.at.Compare(a.at)
	})
	return rows, nil
}

// sample 은 [초(소수), "값"] 쌍이다.
type sample struct {
	at  time.Time
	val string
}

func (s *sample) UnmarshalJSON(b []byte) error {
	var pair []json.RawMessage
	if err := json.Unmarshal(b, &pair); err != nil {
		return err
	}
	if len(pair) != 2 {
		return fmt.Errorf("sample has %d fields", len(pair))
	}
	var sec float64
	if err := json.Unmarshal(pair[0], &sec); err != nil {
		return err
	}
	s.at = time.UnixMilli(int64(math.Round(sec * 1000)))
	return json.Unmarshal(pair[1], &s.val)
}

// series 는 matrix(values)·vector(value) 한 시계열이다.
type series struct {
	Metric map[string]string `json:"metric"`
	Values []sample          `json:"values"`
	Value  *sample           `json:"value"`
}

// seriesRows 는 시계열을 라벨 순으로 묶고, 각 시계열 안은 서버가 준 시간 순 그대로 둔다.
func seriesRows(raw json.RawMessage) ([]labeledRow, error) {
	var ss []series
	if err := json.Unmarshal(raw, &ss); err != nil {
		return nil, err
	}
	slices.SortStableFunc(ss, func(a, b series) int { return cmp.Compare(labelKey(a.Metric), labelKey(b.Metric)) })
	var rows []labeledRow
	for _, s := range ss {
		if s.Value != nil {
			s.Values = append(s.Values, *s.Value)
		}
		for _, v := range s.Values {
			rows = append(rows, labeledRow{at: v.at, labels: s.Metric, last: v.val})
		}
	}
	return rows, nil
}

// labelKey 는 라벨 묶음을 이름순 "k=v," 로 이어 시계열 정렬 키로 쓴다.
func labelKey(m map[string]string) string {
	var b strings.Builder
	for _, k := range slices.Sorted(maps.Keys(m)) {
		b.WriteString(k + "=" + m[k] + ",")
	}
	return b.String()
}
