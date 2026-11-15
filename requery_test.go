package main

import (
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"
)

// ─── R14.5 일반 결과 정렬·거르기 서버로 다시 읽기 ───

func TestRequery_정렬과_거르기_확정은_원래_문장을_감싸_서버에서_다시_읽는다(t *testing.T) {
	m := pgModel(t, "it-local")
	it := pgTest{t, m}
	it.sql("drop table if exists public.rq_t; create table public.rq_t (id int primary key, status text)")
	it.sql("insert into public.rq_t select g, case when g % 2 = 0 then 'PAID' else 'NEW' end from generate_series(1, 1500) g")
	m.cur().conn = 0
	drive(m, m.run("select id, status from public.rq_t order by id"))
	tab := m.cur()
	if tab.res == nil || len(tab.res.Rows) != 1000 || !tab.res.Truncated {
		t.Fatalf("1,000행에서 잘린다: %v %q", tab.res != nil, m.status)
	}
	m.setFocus(fResult)
	tab.grid.FocusCol("id")
	_, cmd := m.Update(key("s")) // 오름
	drive(m, cmd)
	_, cmd = m.Update(key("s")) // 내림
	drive(m, cmd)
	if tab.res.Rows[0][0] != "1500" || tab.grid.Summary() != "정렬 id ↓" {
		t.Fatalf("받은 행 밖의 가장 큰 id 가 맨 위: %q %q", tab.res.Rows[0][0], tab.grid.Summary())
	}
	if !strings.Contains(tab.exportSQL, `ORDER BY "id" DESC NULLS LAST`) || tab.baseSQL != "select id, status from public.rq_t order by id" {
		t.Fatalf("F7 은 감싼 문장, 원래 문장은 남긴다: %q / %q", tab.exportSQL, tab.baseSQL)
	}
	if tab.res.Origin == nil {
		t.Fatal("다시 읽은 결과도 PK 가 있으면 편집할 수 있다")
	}

	m.Update(key("/"))
	for _, r := range "id>1490" {
		_, cmd = m.Update(key(string(r)))
		if cmd != nil {
			drive(m, cmd)
		}
		if tab.running != nil {
			t.Fatal("치는 동안은 서버로 보내지 않는다")
		}
	}
	_, cmd = m.Update(key("enter"))
	drive(m, cmd)
	if len(tab.res.Rows) != 10 || tab.res.Rows[0][0] != "1500" || !strings.Contains(tab.grid.Summary(), "거르기 id>1490") {
		t.Fatalf("Enter 로 서버 거르기: rows=%d %q", len(tab.res.Rows), tab.grid.Summary())
	}

	_, cmd = m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	drive(m, cmd)
	if len(tab.res.Rows) != 1000 || tab.res.Rows[0][0] != "1500" || tab.grid.Summary() != "정렬 id ↓" {
		t.Fatalf("esc 는 거르기만 풀고 다시 읽는다: rows=%d %q", len(tab.res.Rows), tab.grid.Summary())
	}

	// 모든 열 찾기는 서버의 행 글자(_q::text)로 맞춘다. 받은 행 거르기를 다시 돌리면 열 경계에 걸친 글이 사라진다
	m.Update(key("/"))
	m.Update(key("2,PAID"))
	_, cmd = m.Update(key("enter"))
	drive(m, cmd)
	if len(tab.res.Rows) != 150 || !strings.Contains(tab.grid.Summary(), "150/150행") {
		t.Fatalf("서버가 거른 행을 그대로 보인다: res=%d %q %q", len(tab.res.Rows), tab.grid.Summary(), m.status)
	}
	_, cmd = m.Update(tea.KeyMsg{Type: tea.KeyEsc})
	drive(m, cmd)

	// 다시 읽기가 실패하면 보던 결과·정렬을 둔다
	it.sql("drop table public.rq_t")
	_, cmd = m.Update(key("s")) // 내림 → 없음
	drive(m, cmd)
	if !strings.HasPrefix(m.status, "다시 읽지 못함") || len(tab.res.Rows) != 1000 || tab.res.Rows[0][0] != "1500" {
		t.Fatalf("보던 결과를 둔다: %q rows=%d", m.status, len(tab.res.Rows))
	}
	if tab.last == nil || !strings.Contains(tab.last.sql, "DESC") {
		t.Fatal("제출 뒤 새로고침은 실패한 문장이 아니라 마지막으로 성공한 다시 읽기를 보낸다")
	}
}

func TestRequery_여러_문장_결과는_받은_행_안에서만_정렬한다(t *testing.T) {
	m := pgModel(t, "it-local")
	m.cur().conn = 0
	drive(m, m.run("select 1 as a; select g as id from generate_series(1, 3) g"))
	tab := m.cur()
	m.setFocus(fResult)
	tab.grid.FocusCol("id")
	if _, cmd := m.Update(key("s")); cmd != nil || tab.running != nil {
		t.Fatal("문장이 둘이면 서버로 다시 읽지 않는다")
	}
	if tab.grid.Summary() != "정렬 id ↑" {
		t.Fatalf("받은 행 정렬: %q", tab.grid.Summary())
	}
}
