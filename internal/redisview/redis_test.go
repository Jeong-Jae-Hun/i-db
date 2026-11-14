package redisview

import (
	"context"
	"os"
	"reflect"
	"strings"
	"testing"
	"time"

	"github.com/redis/go-redis/v9"
)

// runAll 은 Action 의 명령을 차례로 실행한다. 통합(ui.go)이 같은 세션에서 첫 오류까지 실행하는 것과 같다.
func runAll(t *testing.T, cl *redis.Client, a Action) {
	t.Helper()
	if len(a.Run) == 0 {
		t.Fatalf("실행할 명령이 없다: %+v", a)
	}
	for _, argv := range a.Run {
		if err := do(cl, argv); err != nil {
			t.Fatalf("%q: %v", argv, err)
		}
	}
}

func do(cl *redis.Client, argv []string) error {
	args := make([]any, len(argv))
	for i, s := range argv {
		args[i] = s
	}
	return cl.Do(context.Background(), args...).Err()
}

func TestRedis_만든_명령을_실제_서버에서_실행하면_뜻대로_바뀐다(t *testing.T) {
	addr := os.Getenv("DBTUI_TEST_REDIS_ADDR")
	if addr == "" {
		t.Skip("DBTUI_TEST_REDIS_ADDR not set")
	}
	ctx := context.Background()
	cl := redis.NewClient(&redis.Options{Addr: addr})
	t.Cleanup(func() {
		if err := cl.Close(); err != nil {
			t.Error(err)
		}
	})
	const prefix = "dbtui:redisview:it:"
	keys := []string{prefix + "list", prefix + "race", prefix + "hash", prefix + "str", prefix + "str2", prefix + "zset", prefix + "taken"}
	if err := cl.Del(ctx, keys...).Err(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if err := cl.Del(context.Background(), keys...).Err(); err != nil {
			t.Error(err)
		}
	})

	t.Run("list 의 같은 값 중 고른 칸만 지운다", func(t *testing.T) {
		if err := cl.RPush(ctx, prefix+"list", "x", "y", "x", "y").Err(); err != nil {
			t.Fatal(err)
		}
		_, a := press(newModel(Value{Key: prefix + "list", Type: "list", TTL: -1, Items: []string{"x", "y", "x", "y"}}), "j", "j", "d", "y")
		runAll(t, cl, a)
		got, err := cl.LRange(ctx, prefix+"list", 0, -1).Result()
		if err != nil {
			t.Fatal(err)
		}
		if !reflect.DeepEqual(got, []string{"x", "y", "y"}) {
			t.Fatalf("got %q", got)
		}
	})

	t.Run("읽은 뒤 list 가 바뀌면 편집과 삭제가 실패하고 값을 건드리지 않는다", func(t *testing.T) {
		if err := cl.RPush(ctx, prefix+"race", "a", "b", "c").Err(); err != nil {
			t.Fatal(err)
		}
		read := newModel(Value{Key: prefix + "race", Type: "list", TTL: -1, Items: []string{"a", "b", "c"}})
		_, edit := press(read, "j", "e", "ctrl+u", "B", "enter")
		_, del := press(read, "j", "d", "y")
		// 다른 클라이언트가 앞에 넣어 인덱스 1 이 a 를 가리킨다
		if err := cl.LPush(ctx, prefix+"race", "z").Err(); err != nil {
			t.Fatal(err)
		}
		for _, a := range []Action{edit, del} {
			if len(a.Run) != 1 {
				t.Fatalf("Run %q", a.Run)
			}
			if err := do(cl, a.Run[0]); err == nil || !strings.Contains(err.Error(), "list changed") {
				t.Fatalf("%s: %v", a.Status, err)
			}
		}
		got, err := cl.LRange(ctx, prefix+"race", 0, -1).Result()
		if err != nil {
			t.Fatal(err)
		}
		if !reflect.DeepEqual(got, []string{"z", "a", "b", "c"}) {
			t.Fatalf("got %q", got)
		}
		// 바뀐 list 를 다시 읽어 고치면 그대로 된다
		_, a := press(newModel(Value{Key: prefix + "race", Type: "list", TTL: -1, Items: got}), "j", "j", "e", "ctrl+u", "B", "enter")
		runAll(t, cl, a)
		if got, err = cl.LRange(ctx, prefix+"race", 0, -1).Result(); err != nil || !reflect.DeepEqual(got, []string{"z", "a", "B", "c"}) {
			t.Fatalf("다시 읽은 뒤 편집: %q %v", got, err)
		}
	})

	t.Run("TTL 있는 string 을 고쳐도 TTL 이 남는다", func(t *testing.T) {
		if err := cl.Set(ctx, prefix+"str", "old", time.Hour).Err(); err != nil {
			t.Fatal(err)
		}
		_, a := press(newModel(Value{Key: prefix + "str", Type: "string", TTL: time.Hour, String: "old"}), "e", "ctrl+u", "새\n값", "enter")
		runAll(t, cl, a)
		if v, err := cl.Get(ctx, prefix+"str").Result(); err != nil || v != "새\n값" {
			t.Fatalf("값 %q %v", v, err)
		}
		if ttl, err := cl.TTL(ctx, prefix+"str").Result(); err != nil || ttl <= 0 {
			t.Fatalf("TTL %v %v", ttl, err)
		}
		_, a = press(newModel(Value{Key: prefix + "str", Type: "string", TTL: time.Hour}), "t", "ctrl+u", "enter")
		runAll(t, cl, a)
		if ttl, err := cl.TTL(ctx, prefix+"str").Result(); err != nil || ttl != -1 {
			t.Fatalf("PERSIST 뒤 TTL %v %v", ttl, err)
		}
	})

	t.Run("이름 바꾸기는 있는 키를 덮어쓰지 않는다", func(t *testing.T) {
		if err := cl.Set(ctx, prefix+"taken", "keep", 0).Err(); err != nil {
			t.Fatal(err)
		}
		_, a := press(newModel(Value{Key: prefix + "str", Type: "string", TTL: -1}), "r", "ctrl+u", prefix+"taken", "enter")
		runAll(t, cl, a)
		if v, err := cl.Get(ctx, prefix+"taken").Result(); err != nil || v != "keep" {
			t.Fatalf("덮어썼다: %q %v", v, err)
		}
		_, a = press(newModel(Value{Key: prefix + "str", Type: "string", TTL: -1}), "r", "ctrl+u", prefix+"str2", "enter")
		runAll(t, cl, a)
		if n, err := cl.Exists(ctx, prefix+"str2").Result(); err != nil || n != 1 {
			t.Fatalf("새 이름 %d %v", n, err)
		}
	})

	t.Run("hash 와 zset 편집", func(t *testing.T) {
		if err := cl.HSet(ctx, prefix+"hash", "f", "1").Err(); err != nil {
			t.Fatal(err)
		}
		_, a := press(newModel(Value{Key: prefix + "hash", Type: "hash", TTL: -1, Fields: [][2]string{{"f", "1"}}}), "e", "ctrl+u", "2", "enter")
		runAll(t, cl, a)
		_, a = press(newModel(Value{Key: prefix + "zset", Type: "zset", TTL: -1}), "a", "m", "enter", "+inf", "enter")
		runAll(t, cl, a)
		if v, err := cl.HGet(ctx, prefix+"hash", "f").Result(); err != nil || v != "2" {
			t.Fatalf("hash %q %v", v, err)
		}
		if s, err := cl.ZScore(ctx, prefix+"zset", "m").Result(); err != nil || s < 1e308 {
			t.Fatalf("zset %v %v", s, err)
		}
	})
}
