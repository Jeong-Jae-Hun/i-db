package main

import (
	"fmt"
	"slices"
	"strings"
	"testing"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/x/ansi"

	"github.com/Jeong-Jae-Hun/i-db/internal/complete"
	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/session"
)

// ─── R40.2 팔레트 테이블 · R40.3 최근 테이블 ───

// paletteModel 은 local·stg·prod 접속마다 channels 테이블 카탈로그를 읽은 모델이다. 트리에는 stg·prod 의 public.users 가 있다.
func paletteModel(t *testing.T) *model {
	t.Helper()
	m := testModel(t)
	m.proj.Conns = append(m.proj.Conns, db.Conn{Name: "x-local", UUID: "l", Kind: db.KindPG, Host: "127.0.0.1", Port: 1})
	for i := range m.proj.Conns {
		m.catalogs[i] = &complete.Catalog{Schemas: []complete.Schema{{Name: "public", Tables: []complete.Table{{Name: "channels"}, {Name: "users"}}}}}
	}
	return m
}

func TestPaletteTables_빈_팔레트는_최근_테이블_10개를_최신순으로_동작_위에_보인다(t *testing.T) {
	m := paletteModel(t)
	t0 := time.Date(2026, 11, 15, 9, 0, 0, 0, time.UTC)
	for i := range 12 {
		m.sess.TouchTable("s", "public", fmt.Sprintf("t%02d", i), t0.Add(time.Duration(i)*time.Minute))
	}
	m.sess.TouchTable("gone", "public", "ghost", t0.Add(time.Hour))
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlP})
	list := m.pickerMatches()
	for i := range 10 {
		if want := fmt.Sprintf("[stg] x-stg · public.t%02d", 11-i); list[i] != want {
			t.Fatalf("%d번째: %q, want %q", i, list[i], want)
		}
	}
	if _, ok := m.pickerTables[list[10]]; ok {
		t.Fatalf("11번째부터는 동작이다: %q", list[10])
	}
	if slices.ContainsFunc(m.pickerItems, func(s string) bool { return strings.Contains(s, "ghost") }) {
		t.Fatal(".idea 에 없는 접속의 기록은 뺀다")
	}
	if strings.Contains(strings.Join(list, "\n"), "channels") {
		t.Fatal("최근이 아닌 테이블은 거를 때만 보인다")
	}
	typeText(m, "t00")
	if got := m.pickerMatches(); len(got) != 1 || got[0] != "[stg] x-stg · public.t00" {
		t.Fatalf("10개 밖의 최근 테이블도 거르면 나온다: %v", got)
	}
}

func TestPaletteTables_거르면_환경마다_한_줄씩_최근_것_먼저_나오고_색은_그릴_때만_입힌다(t *testing.T) {
	m := paletteModel(t)
	m.sess.TouchTable("p", "public", "channels", time.Now())
	m.Update(tea.KeyMsg{Type: tea.KeyCtrlP})
	typeText(m, "chan")
	want := []string{"[prod] x-prod · public.channels", "[local] x-local · public.channels", "[stg] x-stg · public.channels"}
	if got := m.pickerMatches(); !slices.Equal(got[:3], want) {
		t.Fatalf("최근 것 먼저, 그다음 local·stg·prod: %v", got)
	}
	m.pickerCursor = 2
	v := m.View()
	if !strings.Contains(v, envStyle(m.conn(0)).Render("[prod]")+" x-prod · public.channels") {
		t.Fatalf("[환경] 에 R6.2 색을 입힌다:\n%q", v)
	}
	if !strings.Contains(ansi.Strip(v), "[stg] x-stg · public.channels") {
		t.Fatal("고른 줄")
	}
}

func TestPaletteTables_Enter는_그_접속의_테이블_보기를_열고_쓰던_탭이면_새_탭에서_연다(t *testing.T) {
	m := paletteModel(t)
	addTable(m, 1)
	palette(m, "x-stg · public.users")
	if tb := m.cur(); tb.tv == nil || tb.conn != 1 || tb.viewConn != 1 || tb.title != "users" || len(m.tabs) != 1 {
		t.Fatalf("빈 탭이 stg 테이블 보기가 된다: tv=%v conn=%d tabs=%d status=%q", tb.tv != nil, tb.conn, len(m.tabs), m.status)
	}

	m.cur().tv = nil
	m.cur().editor.SetValue("select keep")
	addTable(m, 0)
	palette(m, "x-prod · public.users")
	if len(m.tabs) != 2 || m.tab != 1 || m.cur().conn != 0 || m.cur().tv == nil {
		t.Fatalf("글이 있는 탭이면 새 탭에서 연다: tabs=%d conn=%d", len(m.tabs), m.cur().conn)
	}
	if m.tabs[0].editor.Value() != "select keep" || m.tabs[0].conn != 1 {
		t.Fatal("원래 탭은 그대로")
	}

	// 동작은 그대로 다시 누른다
	palette(m, "ctrl+t")
	if len(m.tabs) != 3 {
		t.Fatalf("동작은 그대로: tabs=%d", len(m.tabs))
	}
}

func TestRecentTables_트리_찾기_팔레트로_열면_맨_앞에_남기고_하나로_합치며_50개까지다(t *testing.T) {
	m := paletteModel(t)
	addTable(m, 1)
	m.setFocus(fTree)
	m.Update(key("enter"))
	if got := m.sess.RecentTables; len(got) != 1 || got[0] != (session.RecentTable{Conn: "s", Schema: "public", Table: "users", At: got[0].At}) {
		t.Fatalf("트리 Enter: %+v", got)
	}
	m.newTab()
	addTable(m, 0)
	m.Update(keyMsgOf("ctrl+n"))
	typeText(m, "public.users") // 찾기는 접속 순서대로라 x-prod 가 먼저다
	m.Update(key("enter"))
	m.newTab()
	palette(m, "[stg] x-stg · public.users")
	if got := m.sess.RecentTables; len(got) != 2 || got[0].Conn != "s" || got[1].Conn != "p" || got[1].Table != "users" {
		t.Fatalf("찾기·팔레트, 같은 테이블은 하나로: %+v", got)
	}
	for i := range 60 {
		m.openTable(m.cur(), 1, db.TableRef{Schema: "public", Table: fmt.Sprintf("t%d", i)}, "")
	}
	if got := m.sess.RecentTables; len(got) != 50 || got[0].Table != "t59" {
		t.Fatalf("50개까지: n=%d first=%s", len(got), got[0].Table)
	}
}

func TestRecentTables_DB_RESTORE가_0이어도_세션에_남는다(t *testing.T) {
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	t.Setenv("DB_RESTORE", "0")
	m := paletteModel(t)
	addTable(m, 1)
	m.setFocus(fTree)
	m.Update(key("enter"))
	must(t, m.saveSession())
	r := newModel(m.proj)
	r.restoreSession()
	if got := r.sess.RecentTables; len(got) != 1 || got[0].Conn != "s" || got[0].Table != "users" {
		t.Fatalf("되살린 최근 테이블: %+v", got)
	}
}
