package main

import (
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"slices"
	"strconv"
	"strings"
	"time"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/history"
)

// ─── db history (R11.5) ───
// 화면의 실행 기록과 결과 사본을 화면 없이 찍는다. 기록은 프로젝트와 상관없는 개인 상태 폴더 하나라 -p 를 받지 않는다.

const historyUsage = `사용법:
  db history [--conn 접속] [--since 24h] [--limit 50] [--format table|tsv|json]
  db history show <id> [--format table|tsv|json]

  실행 기록(최근 것부터)을 찍는다. --since 는 지금부터 거슬러 볼 기간(1h·24h·7d)이다.
  result 열의 ◆ 는 결과 사본이 있다는 뜻이고 show <id> 가 그 사본을 찍는다(첫 1,000행·1MiB).
  비밀값이 든 문장은 기록 때 이미 가렸고 사본도 없다.

종료 코드: 0 성공 · 1 실행 오류(사본 없음 포함) · 2 사용법
`

var historyFormats = []string{"table", "tsv", "json"}

// jsonHistory 는 db history --format json 의 한 줄이다.
type jsonHistory struct {
	ID        string `json:"id"`
	Time      string `json:"time"`
	Conn      string `json:"conn"`
	SQL       string `json:"sql"`
	Rows      int    `json:"rows"`
	ElapsedMS int64  `json:"elapsed_ms"`
	Err       string `json:"err,omitempty"`
	Result    bool   `json:"result"`
}

func runHistoryCLI(args []string, stdout, stderr io.Writer, tty bool) int {
	fs := flag.NewFlagSet("db history", flag.ContinueOnError)
	fs.SetOutput(stderr)
	fs.Usage = func() { fmt.Fprint(stderr, historyUsage) }
	var since durFlag
	conn := fs.String("conn", "", "이 접속의 기록만")
	fs.StringVar(conn, "c", "", "--conn 과 같음")
	fs.Var(&since, "since", "지금부터 거슬러 볼 기간(1h·24h·7d)")
	limit := fs.Int("limit", 50, "최대 줄 수(0 이면 전부)")
	format := fs.String("format", "", "출력 형식 table|tsv|json (기본: 터미널이면 table, 아니면 tsv)")
	pos, err := parseInterleaved(fs, args)
	if err != nil {
		if errors.Is(err, flag.ErrHelp) {
			return exitOK
		}
		return exitUsage
	}
	switch {
	case *format == "" && tty:
		*format = "table"
	case *format == "":
		*format = "tsv"
	case !slices.Contains(historyFormats, *format):
		fmt.Fprintf(stderr, "db history: --format must be one of %s\n", strings.Join(historyFormats, "|"))
		return exitUsage
	}
	if *limit < 0 {
		fmt.Fprintln(stderr, "db history: --limit must be 0 or more")
		return exitUsage
	}
	show := len(pos) > 0 && pos[0] == "show"
	if (show && len(pos) != 2) || (!show && len(pos) > 0) {
		fmt.Fprint(stderr, historyUsage)
		return exitUsage
	}
	hp, err := history.DefaultPath()
	if err != nil {
		fmt.Fprintln(stderr, err)
		return exitErr
	}
	o := cliOpts{cmd: "history", format: *format}
	if show {
		return historyShow(hp, pos[1], o, stdout, stderr)
	}
	st, err := history.Open(hp)
	if err != nil {
		fmt.Fprintln(stderr, err)
		return exitErr
	}
	f := history.Filter{Conn: *conn, Limit: *limit}
	if since > 0 {
		f.From = time.Now().Add(-time.Duration(since))
	}
	// 읽기만 하므로 사본 폴더를 만들지 않는다. 없으면(DB_HISTORY_RESULTS=0 로만 썼거나 아직 없음) ◆ 없이 찍는다
	results, _ := history.ReadResults(history.ResultsDir(hp))
	entries := st.List(f)
	if *format == "json" {
		out := make([]jsonHistory, len(entries))
		for i, e := range entries {
			out[i] = jsonHistory{ID: e.ID, Time: e.Time.Format(time.RFC3339), Conn: e.Conn, SQL: db.MaskSecrets(e.SQL), Rows: e.Rows,
				ElapsedMS: e.Elapsed.Milliseconds(), Err: e.Err, Result: e.Result && results != nil && results.Has(e.ID)}
		}
		enc := json.NewEncoder(stdout)
		enc.SetEscapeHTML(false)
		if err := enc.Encode(out); err != nil {
			fmt.Fprintln(stderr, err)
			return exitErr
		}
		return exitOK
	}
	res := &db.Result{Cols: []string{"id", "time", "conn", "rows", "elapsed", "result", "sql", "err"}}
	for _, e := range entries {
		mark := ""
		if e.Result && results != nil && results.Has(e.ID) {
			mark = "◆"
		}
		res.Rows = append(res.Rows, []string{e.ID, e.Time.Local().Format("2006-01-02 15:04:05"), e.Conn, strconv.Itoa(e.Rows),
			fmt.Sprintf("%.2fs", e.Elapsed.Seconds()), mark, oneLine(db.MaskSecrets(e.SQL)), oneLine(e.Err)})
	}
	if err := writeResults(stdout, stderr, o, []cliResult{{res: res}}); err != nil {
		fmt.Fprintln(stderr, err)
		return exitErr
	}
	return exitOK
}

// historyShow 는 사본 하나를 db query 와 같은 꼴로 찍는다. table 은 위에 시각·접속·SQL 을 붙인다.
func historyShow(hp, id string, o cliOpts, stdout, stderr io.Writer) int {
	var s history.Snapshot
	results, err := history.ReadResults(history.ResultsDir(hp))
	if err == nil {
		s, err = results.Read(id)
	}
	switch {
	case errors.Is(err, os.ErrNotExist):
		fmt.Fprintf(stderr, "결과 사본이 없음: %s\n", id)
		return exitErr
	case err != nil:
		fmt.Fprintln(stderr, err)
		return exitErr
	}
	if o.format == "table" {
		fmt.Fprintf(stdout, "-- %s · %s\n%s\n", s.Time.Local().Format("2006-01-02 15:04:05"), s.Conn, db.MaskSecrets(s.SQL))
	}
	if s.Truncated {
		fmt.Fprintf(stderr, "사본은 앞 %d행만 (받은 행 %d)\n", len(s.Rows), s.RowCount)
	}
	res := &db.Result{Cols: s.Cols, Types: s.Types, Rows: s.Rows, Text: s.Text, Elapsed: s.Elapsed, Truncated: s.ResultTruncated}
	if err := writeResults(stdout, stderr, o, []cliResult{{conn: db.Conn{Name: s.Conn}, res: res}}); err != nil {
		fmt.Fprintln(stderr, err)
		return exitErr
	}
	return exitOK
}
