package db

import (
	"context"
	"errors"
	"time"
)

// unlinkBatchTimeout 은 보낸 UNLINK 배치 하나의 응답을 기다리는 시간이다. 500개 UNLINK 는 보통 밀리초 안에 끝난다.
const unlinkBatchTimeout = 10 * time.Second

// 패턴 일괄 삭제(명세 R42)는 트리 조회처럼 풀 클라이언트로 보내 편집기 세션의 SELECT 와 섞이지 않는다.

// ScanKeys 는 SCAN MATCH 로 pattern 에 맞는 키를 처음 나온 순서대로 limit 개까지 모은다.
// SCAN 이 리해시 중 같은 키를 두 번 줘도 한 번만 싣는다. truncated 는 limit 개 뒤에 다른 키가 실제로 더 있다는 뜻이라
// limit 개를 채운 뒤에도 새 키 하나를 찾거나 커서가 끝날 때까지 훑는다.
func (r *RedisConn) ScanKeys(ctx context.Context, pattern string, limit int) (keys []string, truncated bool, err error) {
	if limit <= 0 {
		return nil, false, errors.New("limit must be positive")
	}
	seen := map[string]bool{}
	var cursor uint64
	for {
		batch, next, err := r.cl.Scan(ctx, cursor, pattern, 1000).Result()
		if err != nil {
			return nil, false, err
		}
		for _, k := range batch {
			if seen[k] {
				continue
			}
			if len(keys) == limit {
				return keys, true, nil
			}
			seen[k] = true
			keys = append(keys, k)
		}
		if cursor = next; cursor == 0 {
			return keys, false, nil
		}
	}
}

// UnlinkKeys 는 keys 를 batch 개씩 UNLINK 하고 배치마다 지금까지 지운 수로 progress 를 부른다(nil 이면 안 부름).
// 서버가 UNLINK 를 모르거나 ACL 이 막으면 DEL 로 바꿔 같은 배치부터 다시 보낸다. 배치 사이에 ctx 가 끝나면
// 멈추고 그때까지 지운 수와 ctx 오류를 돌려준다. 지운 수는 응답의 합이라 그사이 사라진 키는 세지 않는다.
// 보낸 배치는 ctx 가 끝나도 응답까지 기다린다. 응답 전에 끊으면 서버는 지웠는데 세지 못한 키가 생기기 때문이다.
// 기다림은 배치마다 unlinkBatchTimeout 으로 끊는다.
func (r *RedisConn) UnlinkKeys(ctx context.Context, keys []string, batch int, progress func(done int)) (int, error) {
	if batch <= 0 {
		return 0, errors.New("batch must be positive")
	}
	done, useDel := 0, false
	for start := 0; start < len(keys); {
		if err := ctx.Err(); err != nil {
			return done, err
		}
		chunk := keys[start:min(start+batch, len(keys))]
		bctx, cancel := context.WithTimeout(context.WithoutCancel(ctx), unlinkBatchTimeout)
		var n int64
		var err error
		if useDel {
			n, err = r.cl.Del(bctx, chunk...).Result()
		} else {
			n, err = r.cl.Unlink(bctx, chunk...).Result()
		}
		cancel()
		if !useDel && commandUnavailable(err) {
			useDel = true
			continue
		}
		if err != nil {
			return done, err
		}
		done += int(n)
		start += len(chunk)
		if progress != nil {
			progress(done)
		}
	}
	return done, nil
}
