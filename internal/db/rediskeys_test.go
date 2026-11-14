package db

import (
	"context"
	"errors"
	"fmt"
	"slices"
	"strings"
	"testing"

	"github.com/redis/go-redis/v9"
)

// v5dbRedis 는 DB 8 접속이다. 다른 시험이 DB 0 을 FLUSHDB 해서 v5db: 키는 따로 둔다.
func v5dbRedis(t *testing.T) *RedisConn {
	t.Helper()
	c := testRedisConn(t)
	c.UUID, c.DB = "it-redis-v5db", "8"
	cs := NewClients()
	t.Cleanup(cs.Close)
	rc, err := cs.Redis(ctx(t), c)
	must(t, err)
	return rc
}

// seedKeys 는 prefix0..n-1 키를 만들고 시험이 끝나면 지운다.
func seedKeys(t *testing.T, rc *RedisConn, prefix string, n int) []string {
	t.Helper()
	keys := make([]string, n)
	for i := range keys {
		keys[i] = fmt.Sprintf("%s%03d", prefix, i)
	}
	cleanupKeys(t, rc, keys...)
	for _, k := range keys {
		must(t, rc.cl.Set(ctx(t), k, "1", 0).Err())
	}
	return keys
}

func TestScanKeys_패턴에_맞는_키를_중복_없이_상한까지_모으고_더_있는지_알린다(t *testing.T) {
	rc := v5dbRedis(t)
	keys := seedKeys(t, rc, "v5db:scan:", 30)
	seedKeys(t, rc, "v5db:other:", 5)

	got, truncated, err := rc.ScanKeys(ctx(t), "v5db:scan:*", 100)
	must(t, err)
	sorted := slices.Sorted(slices.Values(got))
	if truncated || !slices.Equal(sorted, keys) {
		t.Fatalf("전부: %v truncated=%v", got, truncated)
	}
	if got, truncated, err = rc.ScanKeys(ctx(t), "v5db:scan:*", 30); err != nil || truncated || len(got) != 30 {
		t.Fatalf("상한과 같은 개수는 잘림이 아니다: %d %v %v", len(got), truncated, err)
	}
	got, truncated, err = rc.ScanKeys(ctx(t), "v5db:scan:*", 10)
	must(t, err)
	if !truncated || len(got) != 10 || len(slices.Compact(slices.Sorted(slices.Values(got)))) != 10 {
		t.Fatalf("상한: %v truncated=%v", got, truncated)
	}
	for _, k := range got {
		if !strings.HasPrefix(k, "v5db:scan:") {
			t.Fatalf("패턴 밖 키: %q", k)
		}
	}
	if _, _, err := rc.ScanKeys(ctx(t), "*", 0); err == nil {
		t.Fatal("상한 0 은 오류")
	}
}

func TestUnlinkKeys_배치마다_진행을_알리고_응답_합을_지운_수로_돌려준다(t *testing.T) {
	rc := v5dbRedis(t)
	keys := seedKeys(t, rc, "v5db:del:", 25)
	keep := seedKeys(t, rc, "v5db:keep:", 1)
	// 없는 키는 응답이 0 이라 세지 않는다
	var progress []int
	n, err := rc.UnlinkKeys(ctx(t), append(keys, "v5db:del:none"), 10, func(done int) { progress = append(progress, done) })
	must(t, err)
	if n != 25 || !slices.Equal(progress, []int{10, 20, 25}) {
		t.Fatalf("지운 수 %d, 진행 %v", n, progress)
	}
	if left, err := rc.cl.Exists(ctx(t), keys...).Result(); err != nil || left != 0 {
		t.Fatalf("남은 키 %d %v", left, err)
	}
	if left, _ := rc.cl.Exists(ctx(t), keep...).Result(); left != 1 {
		t.Fatal("목록 밖 키를 지웠다")
	}
	if _, err := rc.UnlinkKeys(ctx(t), keys, 0, nil); err == nil {
		t.Fatal("배치 0 은 오류")
	}
}

func TestUnlinkKeys_취소하면_배치_사이에서_멈추고_그때까지_지운_수를_돌려준다(t *testing.T) {
	rc := v5dbRedis(t)
	keys := seedKeys(t, rc, "v5db:cancel:", 25)
	c, cancel := context.WithCancel(ctx(t))
	defer cancel()
	n, err := rc.UnlinkKeys(c, keys, 10, func(int) { cancel() })
	if n != 10 || !errors.Is(err, context.Canceled) {
		t.Fatalf("got %d %v", n, err)
	}
	if left, _ := rc.cl.Exists(ctx(t), keys...).Result(); left != 15 {
		t.Fatalf("남은 키 %d", left)
	}
}

// cancelOnSecondUnlink 는 두 번째 UNLINK 를 보내기 직전에 cancel 을 부르는 훅이다. 배치를 보내는 도중 esc 를 누른 경우다.
type cancelOnSecondUnlink struct {
	cancel context.CancelFunc
	seen   int
}

func (h *cancelOnSecondUnlink) DialHook(next redis.DialHook) redis.DialHook { return next }
func (h *cancelOnSecondUnlink) ProcessPipelineHook(next redis.ProcessPipelineHook) redis.ProcessPipelineHook {
	return next
}
func (h *cancelOnSecondUnlink) ProcessHook(next redis.ProcessHook) redis.ProcessHook {
	return func(c context.Context, cmd redis.Cmder) error {
		if cmd.Name() == "unlink" {
			if h.seen++; h.seen == 2 {
				h.cancel()
			}
		}
		return next(c, cmd)
	}
}

func TestUnlinkKeys_보내는_중에_취소해도_보낸_배치의_응답까지_세어_실제로_지운_수와_같다(t *testing.T) {
	rc := v5dbRedis(t)
	keys := seedKeys(t, rc, "v5db:inflight:", 25)
	c, cancel := context.WithCancel(ctx(t))
	defer cancel()
	rc.cl.AddHook(&cancelOnSecondUnlink{cancel: cancel})
	n, err := rc.UnlinkKeys(c, keys, 10, nil)
	left, lerr := rc.cl.Exists(ctx(t), keys...).Result()
	must(t, lerr)
	if !errors.Is(err, context.Canceled) || n != 25-int(left) || n != 20 {
		t.Fatalf("지운 수 %d, 실제 지운 수 %d, err %v", n, 25-left, err)
	}
}

func TestUnlinkKeys_UNLINK_를_못_쓰면_DEL_로_지운다(t *testing.T) {
	rc := v5dbRedis(t)
	keys := seedKeys(t, rc, "v5db:nounlink:", 5)
	// UNLINK 를 막은 ACL 사용자는 관리형 서버에서 명령을 못 쓰는 경우와 같다
	user := "v5db-nounlink"
	must(t, rc.cl.Do(ctx(t), "ACL", "SETUSER", user, "reset", "on", ">v5pass", "~v5db:*", "+@all", "-unlink").Err())
	t.Cleanup(func() { rc.cl.Do(context.Background(), "ACL", "DELUSER", user) })
	denied := &RedisConn{cl: redis.NewClient(&redis.Options{Addr: rc.opts.Addr, Username: user, Password: "v5pass", DB: 8})}
	t.Cleanup(func() { _ = denied.cl.Close() })
	if err := denied.cl.Unlink(ctx(t), "v5db:x").Err(); err == nil {
		t.Fatal("시험 전제: UNLINK 가 막혀야 한다")
	}
	n, err := denied.UnlinkKeys(ctx(t), keys, 2, nil)
	if err != nil || n != 5 {
		t.Fatalf("got %d %v", n, err)
	}
	if left, _ := rc.cl.Exists(ctx(t), keys...).Result(); left != 0 {
		t.Fatalf("남은 키 %d", left)
	}
}
