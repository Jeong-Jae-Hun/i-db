package main

import (
	"context"
	"fmt"
	"strings"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
	"github.com/Jeong-Jae-Hun/i-db/internal/redisview"
)

// ─── Redis 키 트리·값 편집 ───

// redisNodes 는 이름공간 트리를 화면 트리 노드로 바꾼다. 폴더는 이미 다 읽은 노드라 펼칠 때 다시 읽지 않는다.
func redisNodes(ns []redisview.Node, parent *node) []*node {
	out := make([]*node, 0, len(ns))
	for _, rn := range ns {
		n := &node{kind: nKey, label: rn.Name, key: rn.Full, conn: parent.conn, parent: parent, loaded: true}
		if n.label == "" {
			n.label = `""`
		}
		if !rn.IsKey {
			n.kind = nFolder
			n.label = fmt.Sprintf("%s (%d)", n.label, rn.Count)
			n.children = redisNodes(rn.Children, n)
		}
		out = append(out, n)
	}
	return out
}

// redisKeys 는 n 아래 읽혀 있는 키 이름을 모은다(자동완성·찾기 후보).
func redisKeys(n *node) []string {
	var keys []string
	for _, c := range n.children {
		switch c.kind {
		case nKey:
			keys = append(keys, c.key)
		case nFolder:
			keys = append(keys, redisKeys(c)...)
		}
	}
	return keys
}

// findKey 는 n 아래에서 키 노드를 찾는다.
func findKey(n *node, key string) *node {
	for _, c := range n.children {
		if c.kind == nKey && c.key == key {
			return c
		}
		if c.kind == nFolder && strings.HasPrefix(key, c.key) {
			if f := findKey(c, key); f != nil {
				return f
			}
		}
	}
	return nil
}

// connNode 는 접속 i 의 트리 노드다.
func (m *model) connNode(i int) *node {
	for _, g := range m.roots {
		for _, cn := range g.children {
			if cn.kind == nConn && cn.conn == i {
				return cn
			}
		}
	}
	return nil
}

// loadRedisValue 는 키를 잘리지 않은 값으로 읽어 값 편집기를 연다. 화면용 GetValue 는 칸을 64KiB 에서 잘라 편집에 못 쓴다.
func (m *model) loadRedisValue(t *queryTab, key string) tea.Cmd {
	id, conn, c, clients, life := t.id, t.conn, m.conn(t.conn), m.clients, m.life
	return func() tea.Msg {
		ctx, cancel := context.WithTimeout(life, loadTimeout)
		defer cancel()
		rc, err := clients.Redis(ctx, c)
		if err != nil {
			return redisValueMsg{tab: id, conn: conn, err: err}
		}
		v, err := rc.GetRawValue(ctx, key)
		return redisValueMsg{tab: id, conn: conn, v: v, err: err}
	}
}

// redisEditKey 는 값 편집기에 키를 넘기고, 확정된 명령은 prod 확인을 거쳐 편집기 세션에서 실행한다.
func (m *model) redisEditKey(k tea.KeyMsg) tea.Cmd {
	t := m.cur()
	ed, a := t.redis.Update(k)
	t.redis = &ed
	if a.Status != "" {
		m.status = a.Status
	}
	if len(a.Run) == 0 || m.staleView(t) {
		return nil
	}
	readOnly := true
	lines := make([]string, len(a.Run))
	for i, args := range a.Run {
		readOnly = readOnly && db.IsReadOnlyRedis(args)
		// 기록에는 값 없이 명령과 키만 남긴다
		lines[i] = "-- Redis 편집: " + strings.Join(args[:min(2, len(args))], " ")
	}
	all := &[]*db.Result{}
	return m.guard(pendingRun{tab: t.id, conn: t.conn, kind: rkRedisWrite, readOnly: readOnly, stmts: len(a.Run),
		sql: strings.Join(lines, "\n"), all: all, key: a.Key, old: t.redisKey,
		exec: execScript(m.clients, script{conn: m.conn(t.conn), cmds: a.Run, out: all})})
}

// afterRedisWrite 는 값 편집 명령이 끝난 뒤 키를 다시 읽고, 이름을 바꿨거나 지웠으면 트리도 다시 읽는다.
// 이름 바꾸기는 RENAMENX 하나라, 결과가 0 이면 같은 이름의 키가 있어 바뀌지 않았으므로 원래 키를 다시 읽는다.
func (m *model) afterRedisWrite(t *queryTab, msg resultMsg) (string, tea.Cmd) {
	key, status := msg.key, "반영함"
	if key != msg.old && key != "" && len(msg.all) == 1 && strings.TrimSpace(msg.all[0].Text) == "0" {
		key, status = msg.old, "같은 이름의 키가 있어 바꾸지 않음"
	}
	var cmds []tea.Cmd
	if key != msg.old {
		if cn := m.connNode(msg.conn); cn != nil {
			cn.loaded = false
			cmds = append(cmds, m.expand(cn))
		}
	}
	if key == "" {
		t.redis = nil
		return "키를 지움", tea.Batch(cmds...)
	}
	cmds = append(cmds, m.loadRedisValue(t, key))
	return status, tea.Batch(cmds...)
}
