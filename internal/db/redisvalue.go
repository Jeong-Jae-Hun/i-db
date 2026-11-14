package db

import (
	"context"
	"errors"
	"fmt"
	"strconv"
	"time"
)

// 값 편집용 읽기는 화면용(GetValue)과 달리 칸을 64KiB 에서 자르지 않는다. 잘린 값을 고쳐 저장하면 뒷부분이 사라지기 때문이다.
const maxRawItems = MaxRows

var errNoKey = errors.New("key does not exist")

// RedisValue 는 편집할 키 하나의 잘리지 않은 값이다. 필드 순서·타입은 redisview.Value 와 같아 화면이 그대로 변환한다.
// Truncated 는 string 이 1MiB 를 넘었거나 컬렉션 항목이 1,000개를 넘어 일부만 읽었다는 뜻이다.
type RedisValue struct {
	Key       string
	Type      string
	TTL       time.Duration // 음수면 만료 없음
	String    string
	Fields    [][2]string
	Items     []string
	Members   []string
	ZMembers  [][2]string
	Truncated bool
}

// GetRawValue 는 값 편집기에 넣을 키를 읽는다. string 은 1MiB, 컬렉션은 1,000개까지다.
// 한계: 원소 하나의 크기는 막지 않는다(Redis 에 원소 일부만 읽는 명령이 없다).
func (r *RedisConn) GetRawValue(ctx context.Context, key string) (*RedisValue, error) {
	typ, err := r.cl.Type(ctx, key).Result()
	if err != nil {
		return nil, err
	}
	if typ == "none" {
		return nil, errNoKey
	}
	ttl, err := r.cl.TTL(ctx, key).Result()
	if err != nil {
		return nil, err
	}
	v := &RedisValue{Key: key, Type: typ, TTL: ttl}
	switch typ {
	case "string":
		size, err := r.cl.StrLen(ctx, key).Result()
		if err != nil {
			return nil, err
		}
		if v.String, err = r.cl.GetRange(ctx, key, 0, maxStringBytes-1).Result(); err != nil {
			return nil, err
		}
		v.Truncated = size > maxStringBytes
	case "hash":
		var cursor uint64
		for {
			kv, next, err := r.cl.HScan(ctx, key, cursor, "*", 500).Result()
			if err != nil {
				return nil, err
			}
			for i := 0; i+1 < len(kv); i += 2 {
				v.Fields = append(v.Fields, [2]string{kv[i], kv[i+1]})
			}
			cursor = next
			if cursor == 0 || len(v.Fields) >= maxRawItems {
				v.Truncated = cursor != 0 || len(v.Fields) > maxRawItems
				v.Fields = v.Fields[:min(len(v.Fields), maxRawItems)]
				break
			}
		}
	case "list":
		// 하나 더 받아 정확히 1,000개와 잘림을 가른다
		if v.Items, err = r.cl.LRange(ctx, key, 0, maxRawItems).Result(); err != nil {
			return nil, err
		}
		if len(v.Items) > maxRawItems {
			v.Items, v.Truncated = v.Items[:maxRawItems], true
		}
	case "set":
		var cursor uint64
		for {
			ms, next, err := r.cl.SScan(ctx, key, cursor, "*", 500).Result()
			if err != nil {
				return nil, err
			}
			v.Members = append(v.Members, ms...)
			cursor = next
			if cursor == 0 || len(v.Members) >= maxRawItems {
				v.Truncated = cursor != 0 || len(v.Members) > maxRawItems
				v.Members = v.Members[:min(len(v.Members), maxRawItems)]
				break
			}
		}
	case "zset":
		zs, err := r.cl.ZRangeWithScores(ctx, key, 0, maxRawItems).Result()
		if err != nil {
			return nil, err
		}
		if len(zs) > maxRawItems {
			zs, v.Truncated = zs[:maxRawItems], true
		}
		for _, z := range zs {
			v.ZMembers = append(v.ZMembers, [2]string{fmt.Sprint(z.Member), strconv.FormatFloat(z.Score, 'f', -1, 64)})
		}
	}
	return v, nil
}
