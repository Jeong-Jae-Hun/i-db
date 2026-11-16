package clipboard

import (
	"bytes"
	"context"
	"encoding/base64"
	"errors"
	"slices"
	"strings"
	"testing"
)

type call struct {
	name string
	args []string
	in   []byte
}

// fakeEnv 는 OS·환경 변수·PATH·명령 실행을 바꿔 끼운다. 실제 클립보드 명령은 부르지 않는다.
func fakeEnv(t *testing.T, os string, env map[string]string, onPath []string, wsl bool, fail error) *[]call {
	t.Helper()
	oldRun, oldLook, oldOS, oldEnv, oldExists := run, lookPath, goos, getenv, exists
	t.Cleanup(func() { run, lookPath, goos, getenv, exists = oldRun, oldLook, oldOS, oldEnv, oldExists })
	calls := &[]call{}
	goos = os
	getenv = func(k string) string { return env[k] }
	exists = func(p string) bool { return wsl && p == wslMarker }
	lookPath = func(name string) (string, error) {
		if slices.Contains(onPath, name) {
			return "/bin/" + name, nil
		}
		return "", errors.New("not found")
	}
	run = func(ctx context.Context, name string, args []string, in []byte) error {
		if _, ok := ctx.Deadline(); !ok {
			t.Fatal("시간 제한 없이 명령을 불렀다")
		}
		*calls = append(*calls, call{name, args, in})
		return fail
	}
	return calls
}

func osc(s string) string {
	return "\x1b]52;c;" + base64.StdEncoding.EncodeToString([]byte(s)) + "\a"
}

type failWriter struct{}

func (failWriter) Write([]byte) (int, error) { return 0, errors.New("closed") }

func TestSystemCommand_OS마다_클립보드_명령을_고른다(t *testing.T) {
	all := []string{"pbcopy", "cmd.exe", wslCmd, "wl-copy", "xclip"}
	cases := []struct {
		name   string
		os     string
		env    map[string]string
		onPath []string
		wsl    bool
		want   string
	}{
		{"맥은 pbcopy", "darwin", nil, all, false, "pbcopy"},
		{"WSL 환경 변수면 cmd.exe 로 clip", "linux", map[string]string{"WSL_DISTRO_NAME": "Ubuntu"}, all, false, "cmd.exe"},
		{"WSLInterop 파일이면 cmd.exe 로 clip", "linux", nil, all, true, "cmd.exe"},
		{"Windows 는 cmd.exe 로 clip", "windows", nil, all, false, "cmd.exe"},
		{"WSL PATH 에 Windows 가 없으면 /mnt/c 의 cmd.exe", "linux", nil, []string{wslCmd, "xclip"}, true, wslCmd},
		{"Wayland 면 wl-copy", "linux", map[string]string{"WAYLAND_DISPLAY": "wayland-0"}, all, false, "wl-copy"},
		{"X 는 xclip", "linux", nil, all, false, "xclip"},
		{"xclip 이 없으면 wl-copy", "linux", nil, []string{"wl-copy"}, false, "wl-copy"},
		{"명령이 없으면 없음", "linux", nil, nil, false, ""},
		{"WSL 에 cmd.exe 가 없으면 없음", "linux", nil, []string{"xclip"}, true, ""},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			fakeEnv(t, c.os, c.env, c.onPath, c.wsl, nil)
			got, ok := systemCommand()
			if got.name != c.want || ok != (c.want != "") {
				t.Fatalf("got %+v ok=%v", got, ok)
			}
			if got.name == "xclip" && strings.Join(got.args, " ") != "-selection clipboard" {
				t.Fatalf("xclip 인자: %v", got.args)
			}
		})
	}
}

func TestMode_DB_CLIPBOARD로_고르고_모르는_값은_둘_다다(t *testing.T) {
	for v, want := range map[string]string{"": ModeBoth, "osc52": ModeOSC52, "SYSTEM": ModeSystem, "both": ModeBoth, "x": ModeBoth} {
		fakeEnv(t, "linux", map[string]string{"DB_CLIPBOARD": v}, nil, false, nil)
		if got := Mode(); got != want {
			t.Fatalf("%q → %q, want %q", v, got, want)
		}
	}
}

func TestWrite_기본은_OSC52와_시스템_클립보드에_모두_쓴다(t *testing.T) {
	calls := fakeEnv(t, "darwin", nil, []string{"pbcopy"}, false, nil)
	var buf bytes.Buffer
	if err := Write(&buf, "한글 a"); err != nil {
		t.Fatal(err)
	}
	if buf.String() != osc("한글 a") {
		t.Fatalf("OSC 52: %q", buf.String())
	}
	if len(*calls) != 1 || (*calls)[0].name != "pbcopy" || string((*calls)[0].in) != "한글 a" {
		t.Fatalf("명령: %+v", *calls)
	}
}

func TestWrite_WSL은_코드_페이지를_UTF8로_바꾼_clip에_UTF8로_보낸다(t *testing.T) {
	calls := fakeEnv(t, "linux", map[string]string{"WSL_DISTRO_NAME": "Ubuntu"}, []string{wslCmd}, false, nil)
	if err := Write(&bytes.Buffer{}, "가"); err != nil {
		t.Fatal(err)
	}
	if len(*calls) != 1 || (*calls)[0].name != wslCmd || strings.Join((*calls)[0].args, " ") != "/c chcp 65001 >nul && clip" || string((*calls)[0].in) != "가" {
		t.Fatalf("clip 호출: %+v", *calls)
	}
}

func TestWrite_osc52_모드는_명령을_부르지_않고_system_모드는_OSC52를_쓰지_않는다(t *testing.T) {
	calls := fakeEnv(t, "darwin", map[string]string{"DB_CLIPBOARD": "osc52"}, []string{"pbcopy"}, false, nil)
	var buf bytes.Buffer
	if err := Write(&buf, "a"); err != nil || buf.String() != osc("a") || len(*calls) != 0 {
		t.Fatalf("osc52: err=%v out=%q calls=%v", err, buf.String(), *calls)
	}
	calls = fakeEnv(t, "darwin", map[string]string{"DB_CLIPBOARD": "system"}, []string{"pbcopy"}, false, nil)
	buf.Reset()
	if err := Write(&buf, "a"); err != nil || buf.Len() != 0 || len(*calls) != 1 {
		t.Fatalf("system: err=%v out=%q calls=%v", err, buf.String(), *calls)
	}
	fakeEnv(t, "linux", map[string]string{"DB_CLIPBOARD": "system"}, nil, false, nil)
	if err := Write(&buf, "a"); err == nil {
		t.Fatal("system 모드에서 명령이 없으면 오류여야 한다")
	}
}

func TestWrite_둘_다일_때_한쪽만_실패하면_조용히_넘기고_둘_다_실패하면_오류다(t *testing.T) {
	fakeEnv(t, "darwin", nil, []string{"pbcopy"}, false, errors.New("exit 1"))
	if err := Write(&bytes.Buffer{}, "a"); err != nil {
		t.Fatalf("명령 실패는 조용히: %v", err)
	}
	fakeEnv(t, "darwin", nil, []string{"pbcopy"}, false, nil)
	if err := Write(failWriter{}, "a"); err != nil {
		t.Fatalf("OSC 52 실패해도 명령이 되면 성공: %v", err)
	}
	fakeEnv(t, "linux", nil, nil, false, nil)
	if err := Write(failWriter{}, "a"); err == nil || !strings.Contains(err.Error(), "closed") {
		t.Fatalf("둘 다 실패: %v", err)
	}
}

func TestWrite_10MB를_넘으면_아무_데도_쓰지_않고_크기를_알린다(t *testing.T) {
	calls := fakeEnv(t, "darwin", nil, []string{"pbcopy"}, false, nil)
	var buf bytes.Buffer
	if err := Write(&buf, strings.Repeat("a", MaxBytes)); err != nil {
		t.Fatalf("딱 10MB 는 복사: %v", err)
	}
	buf.Reset()
	*calls = nil
	err := Write(&buf, strings.Repeat("a", MaxBytes+1<<19))
	var big TooBigError
	if !errors.As(err, &big) || buf.Len() != 0 || len(*calls) != 0 {
		t.Fatalf("err=%v out=%d calls=%d", err, buf.Len(), len(*calls))
	}
	if got := big.Status(); got != "너무 커서 복사하지 않음 (10.5 MB) — e/E 로 내보내기" {
		t.Fatalf("상태: %q", got)
	}
}
