//go:build !(linux || darwin || freebsd || netbsd || openbsd || dragonfly)

package keys

import "os"

// inputReady 는 select 를 못 쓰는 플랫폼에서 늘 거짓이다. 잘린 꼬리는 기다리지 않고 그대로 내보낸다.
func inputReady(_ *os.File) bool {
	return false
}
