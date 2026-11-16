// Package clipboard 는 모든 복사 경로(결과 표·값 보기·편집기)가 쓰는 클립보드 쓰기다(R71.4).
// OSC 52 로 터미널 클립보드에 쓰고, 시스템 클립보드 명령(pbcopy·clip.exe·wl-copy·xclip)이 있으면 그쪽에도 쓴다.
// OSC 52 를 막은 터미널이 있어서다. DB_CLIPBOARD=osc52·system·both(기본)로 고른다.
package clipboard

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"runtime"
	"strings"
	"time"
	"unicode/utf16"

	"github.com/aymanbagabas/go-osc52/v2"
)

// MaxBytes 보다 큰 글은 복사하지 않는다. 터미널이 OSC 52 를 버리거나 멈추는 크기라 파일 내보내기로 돌린다
const MaxBytes = 10 << 20

const (
	ModeOSC52  = "osc52"
	ModeSystem = "system"
	ModeBoth   = "both"

	runTimeout = 2 * time.Second
	wslMarker  = "/proc/sys/fs/binfmt_misc/WSLInterop"
)

// 테스트가 바꿔 끼운다. 실제 클립보드 명령을 테스트에서 부르지 않게 한다
var (
	run      = runCommand
	lookPath = exec.LookPath
	goos     = runtime.GOOS
	getenv   = os.Getenv
	exists   = func(p string) bool { _, err := os.Stat(p); return err == nil }
)

// TooBigError 는 MaxBytes 를 넘어 복사하지 않았다는 오류다. 통합이 상태 줄 글로 바꿔 보인다.
type TooBigError struct{ Bytes int }

func (e TooBigError) Error() string {
	return fmt.Sprintf("too large to copy (%d bytes)", e.Bytes)
}

// Status 는 상태 줄 글이다.
func (e TooBigError) Status() string {
	return fmt.Sprintf("너무 커서 복사하지 않음 (%.1f MB) — e/E 로 내보내기", float64(e.Bytes)/(1<<20))
}

// command 는 시스템 클립보드 명령이다. utf16 이면 stdin 을 BOM 붙은 UTF-16LE 로 보낸다.
type command struct {
	name  string
	args  []string
	utf16 bool
}

// Mode 는 DB_CLIPBOARD 값이다. 모르는 값은 기본(both)이다.
func Mode() string {
	switch m := strings.ToLower(strings.TrimSpace(getenv("DB_CLIPBOARD"))); m {
	case ModeOSC52, ModeSystem:
		return m
	}
	return ModeBoth
}

// Write 는 s 를 클립보드에 쓴다. 명령을 기다리므로(최대 2초) tea.Cmd 고루틴에서 부른다.
// both 에서는 둘 중 하나라도 되면 성공이다. 시스템 명령 실패는 OSC 52 가 됐으면 조용히 넘긴다.
func Write(out io.Writer, s string) error {
	if len(s) > MaxBytes {
		return TooBigError{Bytes: len(s)}
	}
	mode := Mode()
	var oscErr, sysErr error
	if mode != ModeSystem {
		_, oscErr = osc52.New(s).WriteTo(out)
	}
	if mode == ModeOSC52 {
		return oscErr
	}
	if c, ok := systemCommand(); ok {
		in := []byte(s)
		if c.utf16 {
			in = UTF16LE(s)
		}
		ctx, cancel := context.WithTimeout(context.Background(), runTimeout)
		sysErr = run(ctx, c.name, c.args, in)
		cancel()
	} else {
		sysErr = errors.New("no system clipboard command")
	}
	switch {
	case mode == ModeSystem:
		return sysErr
	case oscErr != nil && sysErr != nil:
		return oscErr
	}
	return nil
}

// systemCommand 는 이 OS 의 클립보드 명령을 고른다. WSL 은 리눅스지만 Windows 클립보드(clip.exe)로 보낸다.
func systemCommand() (command, bool) {
	var cands []command
	switch {
	case goos == "darwin":
		cands = []command{{name: "pbcopy"}}
	case goos == "windows", getenv("WSL_DISTRO_NAME") != "" || exists(wslMarker):
		cands = []command{{name: "clip.exe", utf16: true}}
	case getenv("WAYLAND_DISPLAY") != "":
		cands = []command{{name: "wl-copy"}, {name: "xclip", args: []string{"-selection", "clipboard"}}}
	default:
		cands = []command{{name: "xclip", args: []string{"-selection", "clipboard"}}, {name: "wl-copy"}}
	}
	for _, c := range cands {
		if _, err := lookPath(c.name); err == nil {
			return c, true
		}
	}
	return command{}, false
}

// UTF16LE 는 s 를 BOM 붙은 UTF-16LE 로 바꾼다. clip.exe 는 BOM 이 없으면 stdin 을 콘솔 코드 페이지로 읽어 한글이 깨진다.
func UTF16LE(s string) []byte {
	units := utf16.Encode([]rune(s))
	b := make([]byte, 2, 2+2*len(units))
	b[0], b[1] = 0xFF, 0xFE
	for _, u := range units {
		b = append(b, byte(u), byte(u>>8))
	}
	return b
}

// runCommand 는 명령에 in 을 넣고 끝나길 기다린다. stdout·stderr 를 잇지 않는다.
// xclip·wl-copy 는 선택을 들고 있으려 자식을 남기는데, 파이프를 이으면 그 자식이 끝날 때까지 Wait 가 막혀서다.
func runCommand(ctx context.Context, name string, args []string, in []byte) error {
	cmd := exec.CommandContext(ctx, name, args...)
	cmd.Stdin = bytes.NewReader(in)
	// 자식이 stdin 을 물려받아 쥐고 있으면 ctx 가 끝나도 입력 복사가 Wait 를 붙잡는다. 같은 2초 안에 놓게 한다
	cmd.WaitDelay = runTimeout
	return cmd.Run()
}
