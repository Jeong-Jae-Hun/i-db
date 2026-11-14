//go:build linux || darwin || freebsd || netbsd || openbsd || dragonfly

package keys

import (
	"os"

	"golang.org/x/sys/unix"
)

// inputReady 는 escWait 안에 f 에 읽을 바이트가 오는지 본다.
// 맥은 tty 에 poll 이 듣지 않아(cancelreader 도 같은 이유로 select 를 쓴다) select 로 본다.
func inputReady(f *os.File) bool {
	fd := int(f.Fd())
	if fd >= unix.FD_SETSIZE {
		return false
	}
	var set unix.FdSet
	set.Set(fd)
	tv := unix.NsecToTimeval(escWait.Nanoseconds())
	n, err := unix.Select(fd+1, &set, nil, nil, &tv)
	// 오류(EINTR 포함)는 일부러 "안 옴"으로 친다. 꼬리를 그대로 내보낼 뿐 입력은 잃지 않는다
	return err == nil && n > 0
}
