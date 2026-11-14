package chart

import (
	"errors"
	"fmt"
	"strconv"
	"strings"
	"testing"

	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

func plain(t *testing.T, res *db.Result, order []int, cursor, w, h int) []string {
	t.Helper()
	out, err := Render(res, order, cursor, w, h)
	if err != nil {
		t.Fatalf("Render: %v", err)
	}
	return strings.Split(ansi.Strip(out), "\n")
}

func channels() *db.Result {
	return &db.Result{
		Cols:  []string{"channel", "count", "avg"},
		Types: []string{"text", "int8", "numeric"},
		Rows:  [][]string{{"web", "10", "1.5"}, {"ios", "5", "2.5"}, {"android", "NULL", "3"}, {"api", "20", "0.5"}},
	}
}

func TestRender_숫자_열이_없으면_오류다(t *testing.T) {
	res := &db.Result{Cols: []string{"a", "b"}, Types: []string{"text", "uuid"}, Rows: [][]string{{"x", "y"}}}
	if _, err := Render(res, nil, 0, 40, 10); !errors.Is(err, errNoNumeric) || err.Error() != "차트로 그릴 숫자 열이 없음" {
		t.Fatalf("err = %v", err)
	}
	if _, err := Render(nil, nil, 0, 40, 10); !errors.Is(err, errNoNumeric) {
		t.Fatalf("nil 결과 err = %v", err)
	}
}

func TestRender_값_열은_커서가_숫자면_그_열_아니면_첫_숫자_열이다(t *testing.T) {
	lines := plain(t, channels(), nil, 2, 60, 10)
	if lines[0] != "값: avg · 이름표: channel" {
		t.Fatalf("커서 숫자 열: %q", lines[0])
	}
	lines = plain(t, channels(), nil, 0, 60, 10)
	if lines[0] != "값: count · 이름표: channel" {
		t.Fatalf("커서 글자 열: %q", lines[0])
	}
}

func TestRender_NULL과_숫자가_아닌_칸은_건너뛰고_수를_적는다(t *testing.T) {
	res := channels()
	res.Rows = append(res.Rows, []string{"bad", "NaN", "1"}, []string{"x", "abc", "1"})
	lines := plain(t, res, nil, 1, 60, 10)
	if lines[1] != "건너뜀 3" {
		t.Fatalf("건너뜀 줄: %q", lines[1])
	}
	// 막대는 결과 순서 그대로, 건너뛴 행 없이
	var labels []string
	for _, l := range lines[2:] {
		labels = append(labels, strings.Fields(l)[0])
	}
	if strings.Join(labels, ",") != "web,ios,api" {
		t.Fatalf("막대 순서: %v", labels)
	}
	if lines := plain(t, channels(), []int{0, 1, 3}, 1, 60, 10); strings.HasPrefix(lines[1], "건너뜀") {
		t.Fatalf("건너뛸 칸이 없으면 줄도 없다: %q", lines[1])
	}
}

func TestRender_거르기_정렬_순서를_따르고_화면만큼만_막대를_그린다(t *testing.T) {
	lines := plain(t, channels(), []int{3, 0}, 1, 60, 10)
	if len(lines) != 3 || !strings.HasPrefix(lines[1], "api") || !strings.HasPrefix(lines[2], "web") {
		t.Fatalf("순서: %q", lines)
	}
	lines = plain(t, channels(), []int{0, 1, 3}, 1, 60, 2)
	if len(lines) != 2 || !strings.HasPrefix(lines[1], "web") {
		t.Fatalf("높이 2: %q", lines)
	}
}

func TestRender_가장_큰_값이_막대를_가득_채우고_값을_끝에_적는다(t *testing.T) {
	lines := plain(t, channels(), []int{3, 0}, 1, 40, 10)
	api, web := lines[1], lines[2]
	if a, b := strings.Count(api, "█"), strings.Count(web, "█"); a < 2*b || a > 2*b+1 {
		t.Fatalf("20 은 10 의 두 배: %q / %q", api, web)
	}
	if !strings.HasSuffix(api, " 20") || !strings.HasSuffix(web, " 10") {
		t.Fatalf("값 원문: %q / %q", api, web)
	}
	if ansi.StringWidth(api) != 40 || ansi.StringWidth(web) != 40 {
		t.Fatalf("값 열이 맞춰져야 한다: %d %d", ansi.StringWidth(api), ansi.StringWidth(web))
	}
}

func TestPosBar_끝_칸은_8분의_1_단위로_채운다(t *testing.T) {
	for cells, want := range map[float64]string{0: "", 1: "█", 2.5: "██▌", 0.125: "▏", 0.99: "█", 3.875: "███▉"} {
		if got := posBar(cells); got != want {
			t.Errorf("posBar(%v) = %q, want %q", cells, got, want)
		}
	}
	for cells, want := range map[float64]string{0.1: "", 0.5: "▐", 0.8: "█", 2.4: "▐██"} {
		if got := negBar(cells); got != want {
			t.Errorf("negBar(%v) = %q, want %q", cells, got, want)
		}
	}
}

func TestRender_음수는_0축_왼쪽으로_양수는_오른쪽으로_뻗는다(t *testing.T) {
	res := &db.Result{Cols: []string{"name", "delta"}, Types: []string{"text", "int4"}, Rows: [][]string{{"up", "30"}, {"down", "-10"}}}
	lines := plain(t, res, nil, 1, 40, 10)
	up, down := lines[1], lines[2]
	axisUp, axisDown := strings.Index(up, "│"), strings.Index(down, "│")
	if axisUp < 0 || ansi.StringWidth(up[:axisUp]) != ansi.StringWidth(down[:axisDown]) {
		t.Fatalf("0축 자리가 같아야 한다: %q / %q", up, down)
	}
	if strings.Contains(up[:axisUp], "█") || !strings.Contains(up[axisUp:], "█") {
		t.Fatalf("양수는 축 오른쪽: %q", up)
	}
	if !strings.Contains(down[:axisDown], "█") || strings.Contains(down[axisDown+len("│"):len(down)-len("-10")], "█") {
		t.Fatalf("음수는 축 왼쪽: %q", down)
	}

	allNeg := &db.Result{Cols: []string{"name", "v"}, Types: []string{"text", "int4"}, Rows: [][]string{{"a", "-4"}, {"b", "-2"}}}
	lines = plain(t, allNeg, nil, 1, 30, 10)
	if strings.Contains(lines[1], "│") || strings.Count(lines[1], "█") <= strings.Count(lines[2], "█") {
		t.Fatalf("모두 음수면 축 없이 크기만큼: %q / %q", lines[1], lines[2])
	}
}

func TestRender_한글_이름표는_화면_폭으로_자른다(t *testing.T) {
	res := &db.Result{Cols: []string{"채널", "수"}, Types: []string{"text", "int8"}, Rows: [][]string{
		{"아주아주긴한글채널이름입니다정말로", "7"}, {"짧음", "3"}, {"탭\t과\x1b[31m제어", "1"},
	}}
	lines := plain(t, res, nil, 1, 30, 10)
	for _, l := range lines {
		if w := ansi.StringWidth(l); w > 30 {
			t.Errorf("폭 %d > 30: %q", w, l)
		}
	}
	if !strings.Contains(lines[1], "…") {
		t.Errorf("긴 이름표는 … 로 자른다: %q", lines[1])
	}
	out, _ := Render(res, nil, 1, 60, 10)
	if strings.Contains(out, "\x1b[31m") || strings.Contains(out, "\t") {
		t.Errorf("이름표의 제어 문자는 보이는 글자로 바꾼다: %q", out)
	}
}

func TestRender_이름표_열이_없으면_행_번호를_쓴다(t *testing.T) {
	res := &db.Result{Cols: []string{"a", "b"}, Types: []string{"int4", "int4"}, Rows: [][]string{{"1", "2"}, {"3", "4"}}}
	lines := plain(t, res, []int{1, 0}, 1, 40, 10)
	if lines[0] != "값: b · 이름표: #" || !strings.HasPrefix(lines[1], "1 ") || !strings.HasSuffix(lines[1], " 4") {
		t.Fatalf("%q", lines)
	}
}

func TestRender_타입_없는_결과는_값으로_숫자_열을_고른다(t *testing.T) {
	res := &db.Result{Cols: []string{"member", "score"}, Rows: [][]string{{"a", "1.5"}, {"b", "NULL"}, {"c", "3"}}}
	lines := plain(t, res, nil, 0, 40, 10)
	if lines[0] != "값: score · 이름표: member" || lines[1] != "건너뜀 1" {
		t.Fatalf("%q", lines)
	}
}

func timeSeries(n int, typ string) *db.Result {
	res := &db.Result{Cols: []string{"day", "count"}, Types: []string{typ, "int8"}}
	for i := range n {
		res.Rows = append(res.Rows, []string{fmt.Sprintf("2026-%02d-%02d", 1+i/28%12, 1+i%28), strconv.Itoa(i % 17)})
	}
	return res
}

func TestRender_첫_열이_날짜면_선_차트다(t *testing.T) {
	for _, typ := range []string{"date", "timestamp", "timestamptz"} {
		lines := plain(t, timeSeries(10, typ), nil, 1, 40, 8)
		if !hasBraille(lines) {
			t.Errorf("%s: 점자 선이 없다: %q", typ, lines)
		}
		if strings.Contains(strings.Join(lines, ""), "█") {
			t.Errorf("%s: 막대가 아니다: %q", typ, lines)
		}
		if !strings.Contains(lines[1], "9┤") || !strings.Contains(lines[len(lines)-2], "0┤") {
			t.Errorf("%s: y축 최대·최소: %q", typ, lines)
		}
		last := lines[len(lines)-1]
		if !strings.Contains(last, "2026-01-01") || !strings.HasSuffix(last, "2026-01-10") {
			t.Errorf("%s: x축 첫·끝: %q", typ, last)
		}
	}
	// 타입이 없으면 값이 날짜로 읽히는지 본다
	untyped := timeSeries(5, "")
	untyped.Types = nil
	untyped.Rows[0][0] = "2026-01-01 09:00:00+09"
	if lines := plain(t, untyped, nil, 1, 40, 8); !hasBraille(lines) {
		t.Errorf("타입 없는 날짜: %q", lines)
	}
	// 날짜 열이 첫 열이 아니면 막대다
	res := timeSeries(3, "date")
	res.Cols, res.Types = []string{"name", "day", "count"}, []string{"text", "date", "int8"}
	for i, r := range res.Rows {
		res.Rows[i] = []string{"n" + r[1], r[0], r[1]}
	}
	if lines := plain(t, res, nil, 2, 40, 8); hasBraille(lines) {
		t.Errorf("첫 열이 글자면 막대: %q", lines)
	}
}

func hasBraille(lines []string) bool {
	for _, l := range lines {
		for _, r := range l {
			if r > 0x2800 && r <= 0x28ff {
				return true
			}
		}
	}
	return false
}

func TestRender_선은_점이_폭보다_많으면_폭에_맞춰_줄인다(t *testing.T) {
	lines := plain(t, timeSeries(1000, "date"), nil, 1, 30, 8)
	if len(lines) != 8 {
		t.Fatalf("줄 수 %d", len(lines))
	}
	for _, l := range lines {
		if ansi.StringWidth(l) > 30 {
			t.Fatalf("폭 초과: %q", l)
		}
	}
	// 1000 점이 화면 끝까지 퍼진다: 첫 열과 마지막 열 모두 점이 있다
	plot := lines[1 : len(lines)-1]
	firstCol, lastCol := false, false
	for _, l := range plot {
		rs := []rune(l)
		_, after, _ := strings.Cut(string(rs), "┤")
		if after == "" {
			_, after, _ = strings.Cut(string(rs), "│")
		}
		ar := []rune(after)
		firstCol = firstCol || ar[0] != ' '
		lastCol = lastCol || ar[len(ar)-1] != ' '
	}
	if !firstCol || !lastCol {
		t.Fatalf("점이 폭 전체에 퍼져야 한다: %q", plot)
	}
}

func TestRender_어떤_크기에서도_폭과_높이를_넘지_않는다(t *testing.T) {
	results := map[string]*db.Result{"막대": channels(), "선": timeSeries(50, "date")}
	mixed := channels()
	mixed.Rows = append(mixed.Rows, []string{"아주긴한글이름표입니다", "-123456789.123", "-1"})
	results["음수"] = mixed
	for name, res := range results {
		for w := 1; w <= 50; w++ {
			for h := 1; h <= 12; h++ {
				out, err := Render(res, nil, 1, w, h)
				if err != nil {
					t.Fatalf("%s %dx%d: %v", name, w, h, err)
				}
				lines := strings.Split(out, "\n")
				if len(lines) > h {
					t.Fatalf("%s %dx%d: 줄 %d", name, w, h, len(lines))
				}
				for _, l := range lines {
					if ansi.StringWidth(l) > w {
						t.Fatalf("%s %dx%d: 폭 %d %q", name, w, h, ansi.StringWidth(l), ansi.Strip(l))
					}
				}
			}
		}
	}
	if out, err := Render(channels(), nil, 1, 0, 5); out != "" || err != nil {
		t.Fatalf("폭 0: %q %v", out, err)
	}
}

func TestRender_값이_하나뿐이거나_모두_같아도_그린다(t *testing.T) {
	one := timeSeries(1, "date")
	if lines := plain(t, one, nil, 1, 30, 6); !hasBraille(lines) {
		t.Fatalf("점 하나: %q", lines)
	}
	flat := timeSeries(5, "date")
	for _, r := range flat.Rows {
		r[1] = "7"
	}
	if lines := plain(t, flat, nil, 1, 30, 6); !hasBraille(lines) {
		t.Fatalf("평평한 선: %q", lines)
	}
	zero := &db.Result{Cols: []string{"n", "v"}, Types: []string{"text", "int4"}, Rows: [][]string{{"a", "0"}}}
	if lines := plain(t, zero, nil, 1, 30, 6); strings.Contains(lines[1], "█") || !strings.HasSuffix(lines[1], "0") {
		t.Fatalf("0 은 막대 없이 값만: %q", lines)
	}
}

func TestRender_최대_최소_차가_float_범위를_넘어도_패닉_없이_크기를_지킨다(t *testing.T) {
	bar := &db.Result{Cols: []string{"n", "v"}, Types: []string{"text", "float8"},
		Rows: [][]string{{"a", "1e308"}, {"b", "-1e308"}, {"c", "5e307"}, {"d", "0"}}}
	line := timeSeries(4, "date")
	for i, v := range []string{"1e308", "-1e308", "5e307", "-1.7976931348623157e308"} {
		line.Rows[i][1] = v
	}
	for name, res := range map[string]*db.Result{"막대": bar, "선": line} {
		for _, size := range [][2]int{{40, 10}, {12, 4}, {3, 2}} {
			out, err := Render(res, nil, 1, size[0], size[1])
			if err != nil {
				t.Fatalf("%s %v: %v", name, size, err)
			}
			lines := strings.Split(out, "\n")
			if len(lines) > size[1] {
				t.Fatalf("%s %v: 줄 %d", name, size, len(lines))
			}
			for _, l := range lines {
				if ansi.StringWidth(l) > size[0] {
					t.Fatalf("%s %v: 폭 %d %q", name, size, ansi.StringWidth(l), ansi.Strip(l))
				}
			}
		}
	}
}
