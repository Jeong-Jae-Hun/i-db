// Package notify 는 긴 실행이 끝났을 때 터미널 알림(OSC 9)을 보낸다(명세 R30).
package notify

import (
	"fmt"
	"io"
	"os"
	"strconv"
	"time"

	"github.com/Jeong-Jae-Hun/i-db/internal/textutil"
)

const (
	defaultThreshold = 10 * time.Second
	maxThresholdSecs = 24 * 60 * 60
)

// threshold 는 알림을 보낼 최소 실행 시간이다. DB_NOTIFY_SECS(정수 초)로 바꾸고 0 이면 끈다(0 을 돌려줌).
// 숫자가 아니거나 음수거나 24시간을 넘으면 기본 10초다. 상한은 n*time.Second 가 넘치지 않게 하려는 것이다.
func threshold() time.Duration {
	s, ok := os.LookupEnv("DB_NOTIFY_SECS")
	if !ok {
		return defaultThreshold
	}
	n, err := strconv.Atoi(s)
	if err != nil || n < 0 || n > maxThresholdSecs {
		return defaultThreshold
	}
	return time.Duration(n) * time.Second
}

// ShouldNotify 는 명세 R30.1 의 조건이다: 기준 시간 이상 걸렸고, 사용자가 취소하지 않았고, stdout 이 터미널이다.
// 터미널이 아니면 OSC 글이 파일·파이프에 그대로 섞여 들어간다.
func ShouldNotify(elapsed time.Duration, canceled, isTTY bool) bool {
	t := threshold()
	return t > 0 && elapsed >= t && !canceled && isTTY
}

// Done 은 `ESC ] 9 ; db: <접속> 끝 12.3s · 1,001행 BEL` 을 쓴다. 실패면 `실패 12.3s` 다.
// SQL·값은 넣지 않는다. 접속 이름은 설정 파일에서 오므로 제어 문자를 보이는 글자로 바꿔
// ESC·BEL 이 알림을 끊거나 다른 터미널 명령을 끼워 넣지 못하게 한다.
func Done(w io.Writer, conn string, elapsed time.Duration, rows int, failed bool) error {
	msg := fmt.Sprintf("db: %s 끝 %.1fs · %s행", textutil.Printable(conn, false), elapsed.Seconds(), textutil.GroupDigits(int64(max(rows, 0))))
	if failed {
		msg = fmt.Sprintf("db: %s 실패 %.1fs", textutil.Printable(conn, false), elapsed.Seconds())
	}
	_, err := fmt.Fprintf(w, "\x1b]9;%s\a", msg)
	return err
}
