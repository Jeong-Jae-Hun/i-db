package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"strconv"
	"strings"
	"sync"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

// ─── db status (R64.3) ───

// statusRow 는 status 의 접속 한 줄이다. OK 가 nil 이면 확인하지 않은 prod 접속이다(--prod 없음).
type statusRow struct {
	Group     string         `json:"group"`
	Name      string         `json:"name"`
	Env       string         `json:"env"`
	Kind      string         `json:"kind"`
	OK        *bool          `json:"ok"`
	LatencyMS int64          `json:"latency_ms"`
	Version   string         `json:"version"`
	Metrics   []statusMetric `json:"metrics"`
	Error     string         `json:"error,omitempty"`
	Note      string         `json:"note,omitempty"`
}

type statusMetric struct {
	Name  string `json:"name"`
	Value string `json:"value"`
	Warn  bool   `json:"warn"`
}

// runStatus 는 고른 접속(없으면 모두)을 동시에 한 번 확인해 찍는다. prod 는 --prod 가 없으면 연결하지 않고
// `--prod 로 확인` 으로 보인다(R58.2). 확인한 접속이 하나라도 실패하면 1 이다.
func runStatus(ctx context.Context, o cliOpts, proj *db.Project, clients *db.Clients, stdout, stderr io.Writer) int {
	if o.format == "csv" {
		fmt.Fprintln(stderr, "db status: --format must be one of table|tsv|json")
		return exitUsage
	}
	order := connOrder(proj.Conns)
	if names := append(append([]string{}, o.conns...), o.args...); len(names) > 0 {
		order = order[:0:0]
		for _, n := range names {
			i := connByName(proj, n)
			if i < 0 || proj.Conns[i].Kind == "" {
				fmt.Fprintf(stderr, "db status: 접속 없음: %s (db conns)\n", n)
				return exitUsage
			}
			order = append(order, i)
		}
	}
	sources := newSourcePool()
	defer sources.close()
	rows := make([]statusRow, len(order))
	var wg sync.WaitGroup
	for r, i := range order {
		c := proj.Conns[i]
		rows[r] = statusRow{Group: db.GroupOf(c), Name: c.Name, Env: c.Env(), Kind: string(c.Kind), Metrics: []statusMetric{}}
		if c.Env() == "prod" && !o.prod {
			rows[r].Note = dashProdHint
			continue
		}
		wg.Add(1)
		go func() {
			defer wg.Done()
			pctx, cancel := context.WithTimeout(ctx, probeTimeout)
			defer cancel()
			h := probeConn(pctx, clients, sources, c)
			row := &rows[r]
			row.OK, row.LatencyMS, row.Version = &h.OK, h.Latency.Milliseconds(), h.Version
			for _, x := range h.Metrics {
				row.Metrics = append(row.Metrics, statusMetric(x))
			}
			if !h.OK {
				row.Error = cliErrText(pctx, proj, o, errors.New(h.Err), c.Name)
			}
		}()
	}
	wg.Wait()

	failed := false
	for _, r := range rows {
		failed = failed || (r.OK != nil && !*r.OK)
	}
	var err error
	if o.format == "json" {
		enc := json.NewEncoder(stdout)
		enc.SetEscapeHTML(false)
		err = enc.Encode(rows)
	} else {
		res := &db.Result{Cols: []string{"group", "name", "env", "kind", "ok", "latency_ms", "version", "metrics"},
			Types: []string{"", "", "", "", "", "int8", "", ""}}
		for _, r := range rows {
			ok, lat, metrics := "-", "", r.Note
			if r.OK != nil {
				ok, lat = strconv.FormatBool(*r.OK), strconv.FormatInt(r.LatencyMS, 10)
				parts := make([]string, len(r.Metrics))
				for i, x := range r.Metrics {
					parts[i] = strings.TrimSpace(x.Name + " " + x.Value)
					if x.Warn {
						parts[i] += " !"
					}
				}
				metrics = strings.Join(parts, " · ")
				if r.Error != "" {
					metrics = r.Error
				}
			}
			res.Rows = append(res.Rows, []string{r.Group, r.Name, r.Env, r.Kind, ok, lat, r.Version, metrics})
		}
		err = writeResults(stdout, stderr, o, []cliResult{{res: res}})
	}
	if err != nil {
		fmt.Fprintln(stderr, err)
		return exitErr
	}
	if failed {
		return exitErr
	}
	return exitOK
}
