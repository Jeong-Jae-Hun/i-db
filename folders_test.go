package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/Jeong-Jae-Hun/i-db/internal/db"
)

// typeLine 은 열린 한 줄 입력을 v 로 바꾸고 Enter 를 누른다.
func typeLine(m *model, v string) {
	m.input.SetValue(v)
	m.Update(key("enter"))
}

func TestFolder_M_은_DataGrip_접속도_폴더만_옮기고_그룹과_AES_키는_그대로다(t *testing.T) {
	m := connTestModel(t)
	if err := os.WriteFile(filepath.Join(m.proj.Dir, ".env"), []byte("GRP_AES_KEY=k\nGRP_AES_IV=00\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	m.reloadConns()
	i := m.selectConn(t, "dg-stg")
	m.catalogs[i] = nil // 폴더만 옮기면 캐시 자리를 지우지 않는다
	m.Update(key("M"))
	if m.overlay != oPrompt || m.input.Value() != "grp" {
		t.Fatalf("지금 폴더를 채운 입력: overlay=%v %q", m.overlay, m.input.Value())
	}
	typeLine(m, "moved")
	c := m.conn(i)
	if cn := m.connNode(i); cn == nil || cn.parent.label != "moved" || m.current() != cn {
		t.Fatalf("새 폴더에서 그 접속을 고른다: %+v", cn)
	}
	if c.Group != "grp" || c.Folder != "moved" {
		t.Fatalf("그룹은 그대로: %+v", c)
	}
	if key, _ := m.proj.AESKeyIV(c); key != "k" {
		t.Fatal("암호화 키는 그룹으로 찾는다")
	}
	if _, ok := m.catalogs[i]; !ok {
		t.Fatal("폴더만 옮긴 접속은 바뀐 접속으로 보지 않는다")
	}
	if !strings.Contains(m.status, "옮김: dg-stg → moved") {
		t.Fatalf("상태: %q", m.status)
	}
	b, err := os.ReadFile(filepath.Join(m.proj.Dir, ".idea", "dataSources.xml"))
	if err != nil || strings.Contains(string(b), "moved") {
		t.Fatal(".idea 는 고치지 않는다")
	}

	m.selectConn(t, "dg-stg")
	m.Update(key("M"))
	typeLine(m, "")
	if cn := m.connNode(i); cn.parent.label != "grp" || m.conn(i).Folder != "" {
		t.Fatalf("빈 값은 원래 그룹으로: %q", cn.parent.label)
	}
	if b, _ := os.ReadFile(filepath.Join(m.proj.Dir, ".dbtui", "folders.json")); strings.Contains(string(b), "moved") {
		t.Fatalf("되돌리면 기록을 지운다: %s", b)
	}
}

func TestFolder_폴더_줄_E_는_이름을_바꾸고_펼침을_이어받는다(t *testing.T) {
	m := connTestModel(t)
	g := m.groupNode("grp")
	m.rebuild()
	m.selectNode(g)
	m.Update(key("E"))
	if m.overlay != oPrompt || m.input.Value() != "grp" {
		t.Fatalf("폴더 이름 입력: overlay=%v %q", m.overlay, m.input.Value())
	}
	typeLine(m, "renamed")
	ng := m.groupNode("renamed")
	if ng == nil || m.groupNode("grp") != nil || len(ng.children) != 4 || !ng.expanded || m.current() != ng {
		t.Fatalf("네 접속이 새 폴더로: %+v", ng)
	}
	if m.status != "폴더 이름 바꿈: grp → renamed (접속 4)" {
		t.Fatalf("상태: %q", m.status)
	}
	// db 전용 접속도 connections.json 의 group 은 그대로다
	if c := m.conn(connByName(m.proj, "mine-stg")); c.Group != "grp" {
		t.Fatalf("그룹: %q", c.Group)
	}
}

func TestFolder_깨진_folders_json_은_경고하고_덮어쓰지_않는다(t *testing.T) {
	m := connTestModel(t)
	path := filepath.Join(m.proj.Dir, ".dbtui", "folders.json")
	must(t, os.WriteFile(path, []byte("{broken"), 0o644))
	p, err := loadProject(m.proj.Dir)
	must(t, err)
	if len(p.Warnings) == 0 || db.FolderOf(p.Conns[0]) != "grp" {
		t.Fatalf("경고만 하고 그룹대로: %v", p.Warnings)
	}
	if err := db.SetFolder(m.proj.Dir, p.Conns[:1], "x"); err == nil {
		t.Fatal("깨진 파일은 덮지 않는다")
	}
	if err := db.SetFolder(m.proj.Dir, nil, "a\x1bb"); err == nil {
		t.Fatal("제어 문자 폴더는 거절한다")
	}
}

func TestConnCLI_folder_는_옮기고_이름을_바꾸고_되돌린다(t *testing.T) {
	dir := connCLIProject(t)
	if code, out, errw := runCLITest(t, "", "conn", "folder", "dg-stg", "ops", "-p", dir); code != exitOK || out != "옮김: dg-stg → ops\n" {
		t.Fatalf("code=%d out=%q err=%q", code, out, errw)
	}
	_, out, _ := runCLITest(t, "", "conns", "-p", dir, "--format", "tsv")
	if !strings.Contains(out, "dg-stg\tgrp\tpostgresql\tstg\t127.0.0.1\tapp\tdatagrip\tops\n") {
		t.Fatalf("group 은 그대로, folder 열: %q", out)
	}
	if code, out, _ := runCLITest(t, "", "conn", "folder", "--rename", "ops", "infra", "-p", dir); code != exitOK || out != "폴더 이름 바꿈: ops → infra (접속 1)\n" {
		t.Fatalf("rename: %d %q", code, out)
	}
	if code, out, _ := runCLITest(t, "", "conn", "folder", "dg-stg", "--reset", "-p", dir); code != exitOK || out != "되돌림: dg-stg → grp\n" {
		t.Fatalf("reset: %d %q", code, out)
	}
	for _, args := range [][]string{{"nope", "x"}, {"dg-stg"}, {"--rename", "nope", "x"}} {
		if code, _, _ := runCLITest(t, "", append(append([]string{"conn", "folder"}, args...), "-p", dir)...); code != exitUsage {
			t.Fatalf("%v: code=%d", args, code)
		}
	}
}

func TestFolder_옮긴_폴더_줄의_N_은_폴더가_아니라_실제_그룹을_채운다(t *testing.T) {
	m := connTestModel(t)
	must(t, db.SetFolder(m.proj.Dir, m.proj.Conns, "ops"))
	m.reloadConns()
	m.rebuild()
	m.selectNode(m.groupNode("ops"))
	m.Update(key("N"))
	if m.connForm == nil || m.connForm.inputs[cfGroup].Value() != "grp" {
		t.Fatal("암호화 키를 찾는 그룹은 grp 다")
	}
}

func TestConnCLI_folder_rename_의_빈_새_이름은_되돌리지_않고_거절한다(t *testing.T) {
	dir := connCLIProject(t)
	runCLITest(t, "", "conn", "folder", "dg-stg", "ops", "-p", dir)
	if code, _, _ := runCLITest(t, "", "conn", "folder", "--rename", "ops", " ", "-p", dir); code != exitUsage {
		t.Fatalf("code=%d", code)
	}
	_, out, _ := runCLITest(t, "", "conns", "-p", dir, "--format", "tsv")
	if !strings.Contains(out, "\tdatagrip\tops\n") {
		t.Fatalf("그대로 ops: %q", out)
	}
}
