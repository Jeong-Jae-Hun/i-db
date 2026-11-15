package main

import (
	"context"
	"strings"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/finder"
)

// ─── 객체 찾기 ───

// reveal 은 찾은 객체를 트리에서 고른다. 부모를 아직 안 읽었으면 읽기를 걸고, 읽은 뒤(childrenMsg) 다시 부른다.
// 테이블·뷰는 테이블 보기로 연다. 정의로 이동(revealTab, R26)이면 포커스를 그대로 두고 요청한 탭에 구조를 연다.
// ci 는 그 객체의 접속 인덱스다. 이름이 같은 접속이 있어도 고른 접속으로 가게 이름이 아닌 인덱스로 받는다.
func (m *model) reveal(ci int, it finder.Item) tea.Cmd {
	m.jump = nil
	switch it.Kind {
	case finder.KindColumn:
		// 열은 그 테이블을 찾아 테이블 보기로 연다(R56). 이름은 "테이블.열" 이고 테이블 이름에 점이 들 수 있어 마지막 점에서 자른다
		table := it.Name[:max(0, strings.LastIndexByte(it.Name, '.'))]
		it = finder.Item{Conn: it.Conn, Kind: finder.KindTable, Schema: it.Schema, Name: table}
	case finder.KindFunction:
		// 함수는 트리에 노드가 없어 정의로 이동(R26)과 같이 이름으로 찾아 정의를 연다
		if ci < 0 {
			m.status = "접속을 찾지 못함: " + it.Conn
			return nil
		}
		t := m.cur()
		if ci != t.conn && t.bound() {
			t = m.newTab()
		}
		id, name := t.id, it.Name
		return withPG(m.life, m.clients, m.conn(ci), func(ctx context.Context, pg *db.PGConn, err error) tea.Msg {
			if err != nil {
				return defMsg{tab: id, conn: ci, name: name, err: err}
			}
			obj, err := pg.Resolve(ctx, name)
			return defMsg{tab: id, conn: ci, name: name, obj: obj, err: err}
		})
	}
	cn := m.connNode(ci)
	if cn == nil {
		m.status = "접속을 찾지 못함: " + it.Conn
		m.revealTab = 0
		return nil
	}
	if m.filter != "" {
		m.filter = ""
	}
	if !cn.loaded {
		m.jump, m.jumpConn = &it, ci
		return m.expand(cn)
	}
	var target *node
	if it.Kind == finder.KindRedisKey {
		target = findKey(cn, it.Name)
	} else {
		for _, sn := range cn.children {
			if sn.kind != nSchema || sn.label != it.Schema {
				continue
			}
			if !sn.loaded {
				m.jump, m.jumpConn = &it, ci
				return m.expand(sn)
			}
			for _, tn := range sn.children {
				if tn.kind == nTable && tn.label == it.Name {
					target = tn
				}
			}
		}
	}
	describe := m.revealTab
	m.revealTab = 0
	if target == nil {
		m.status = "트리에서 찾지 못함: " + finder.Target(it)
		return nil
	}
	for p := target.parent; p != nil; p = p.parent {
		p.expanded = true
	}
	m.rebuild()
	m.selectNode(target)
	if describe != 0 && target.kind == nTable {
		// 찾는 사이 요청한 탭을 닫았으면 다른 탭에 열지 않는다
		if t := m.findTab(describe); t != nil {
			return m.describe(t, target)
		}
		return nil
	}
	m.setFocus(fTree)
	if target.kind == nTable {
		return m.activate(target, false)
	}
	return nil
}
