package db

import (
	"context"
	"errors"
	"fmt"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/redis/go-redis/v9"
)

func TestRedis_편집용_값은_칸을_자르지_않고_1000개와_1MiB에서만_자른다(t *testing.T) {
	c := testRedisConn(t)
	cs := NewClients()
	defer cs.Close()
	rc, err := cs.Redis(ctx(t), c)
	must(t, err)
	cl := redis.NewClient(&redis.Options{Addr: rc.opts.Addr})
	defer cl.Close()
	big := strings.Repeat("가", 30_000) // 90KB, 화면 칸 상한(64KiB)보다 크다
	keys := []string{"it:raw:s", "it:raw:huge", "it:raw:h", "it:raw:l", "it:raw:z", "it:raw:set"}
	must(t, cl.Del(ctx(t), keys...).Err())
	defer cl.Del(ctx(t), keys...)
	must(t, cl.Set(ctx(t), "it:raw:s", big, time.Minute).Err())
	must(t, cl.Set(ctx(t), "it:raw:huge", strings.Repeat("x", maxStringBytes+5), 0).Err())
	must(t, cl.HSet(ctx(t), "it:raw:h", "f", big).Err())
	items := make([]any, maxRawItems+1)
	for i := range items {
		items[i] = strconv.Itoa(i)
	}
	must(t, cl.RPush(ctx(t), "it:raw:l", items...).Err())
	must(t, cl.ZAdd(ctx(t), "it:raw:z", redis.Z{Member: "m", Score: 1.5}).Err())
	must(t, cl.SAdd(ctx(t), "it:raw:set", "a").Err())

	v, err := rc.GetRawValue(ctx(t), "it:raw:s")
	if err != nil || v.Type != "string" || v.String != big || v.Truncated || v.TTL <= 0 {
		t.Fatalf("string: err=%v type=%s len=%d ttl=%v", err, v.Type, len(v.String), v.TTL)
	}
	v, err = rc.GetRawValue(ctx(t), "it:raw:huge")
	if err != nil || len(v.String) != maxStringBytes || !v.Truncated || v.TTL >= 0 {
		t.Fatalf("1MiB 초과: err=%v len=%d trunc=%v ttl=%v", err, len(v.String), v.Truncated, v.TTL)
	}
	v, err = rc.GetRawValue(ctx(t), "it:raw:h")
	if err != nil || len(v.Fields) != 1 || v.Fields[0][1] != big {
		t.Fatalf("hash 원소는 통째로: err=%v", err)
	}
	v, err = rc.GetRawValue(ctx(t), "it:raw:l")
	if err != nil || len(v.Items) != maxRawItems || !v.Truncated || v.Items[999] != "999" {
		t.Fatalf("list: err=%v n=%d trunc=%v", err, len(v.Items), v.Truncated)
	}
	v, err = rc.GetRawValue(ctx(t), "it:raw:z")
	if err != nil || len(v.ZMembers) != 1 || v.ZMembers[0] != [2]string{"m", "1.5"} {
		t.Fatalf("zset: err=%v %+v", err, v)
	}
	v, err = rc.GetRawValue(ctx(t), "it:raw:set")
	if err != nil || len(v.Members) != 1 || v.Truncated {
		t.Fatalf("set: err=%v %+v", err, v)
	}
	if _, err := rc.GetRawValue(ctx(t), "it:raw:none"); !errors.Is(err, errNoKey) {
		t.Fatalf("없는 키는 오류: %v", err)
	}
}

func TestClients_닫은_뒤의_실행은_막히지_않고_바로_오류다(t *testing.T) {
	pc := testPGConn(t)
	rcConn := testRedisConn(t)
	cs := NewClients()
	pg, err := cs.PG(ctx(t), pc)
	must(t, err)
	_, err = pg.Run(ctx(t), "select 1")
	must(t, err)
	rc, err := cs.Redis(ctx(t), rcConn)
	must(t, err)
	_, err = rc.Run(ctx(t), []string{"PING"})
	must(t, err)
	cs.Close()

	done := make(chan error, 4)
	go func() {
		// 컨텍스트에 마감이 없어도 기다리지 않아야 한다
		bg := context.Background()
		_, e1 := pg.Run(bg, "select 1")
		_, e2 := pg.ApplyChanges(bg, ChangeSet{Changes: []Change{{Op: ChangeDelete}}})
		_, e3 := rc.Run(bg, []string{"PING"})
		_, e4 := cs.PG(bg, pc)
		done <- errors.Join(check(e1), check(e2), check(e3), check(e4))
	}()
	select {
	case err := <-done:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("Close 뒤 실행이 세션 잠금에서 멈췄다")
	}
}

func check(err error) error {
	if !errors.Is(err, errClosed) {
		return fmt.Errorf("want errClosed, got %v", err)
	}
	return nil
}
