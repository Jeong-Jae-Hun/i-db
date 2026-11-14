package mcpserver

import (
	"errors"
	"fmt"
	"strconv"
	"strings"
)

// maxRedisCount 는 SCAN 계열 COUNT·MEMORY USAGE SAMPLES 의 상한이다. 한 번에 서버가 훑는 양을 묶는다.
const maxRedisCount = 1000

const (
	redisSnoopMsg = "%s is not allowed over MCP: it exposes other clients' addresses and commands"
	redisWholeMsg = "%s returns the whole collection; use redis_get or the matching SSCAN/HSCAN with COUNT"
)

var redisDenied = map[string]string{
	"KEYS":     "KEYS blocks the server; use redis_scan instead",
	"CLIENT":   fmt.Sprintf(redisSnoopMsg, "CLIENT"),
	"SLOWLOG":  fmt.Sprintf(redisSnoopMsg, "SLOWLOG"),
	"MONITOR":  fmt.Sprintf(redisSnoopMsg, "MONITOR"),
	"SMEMBERS": fmt.Sprintf(redisWholeMsg, "SMEMBERS"),
	"HGETALL":  fmt.Sprintf(redisWholeMsg, "HGETALL"),
	"HKEYS":    fmt.Sprintf(redisWholeMsg, "HKEYS"),
	"HVALS":    fmt.Sprintf(redisWholeMsg, "HVALS"),
}

// optRule 은 Redis 명령 옵션 하나의 정수 범위다. name 뒤 offset 번째 값을 본다(LIMIT offset count 의 count 는 2).
type optRule struct {
	name     string
	offset   int
	min, max int64
	required bool
}

// checkRedisArgs 는 db.IsReadOnlyRedis 를 통과한 명령 중 다른 클라이언트 정보를 드러내거나
// 응답·서버 작업량에 상한이 없는 것을 거부한다. TUI 는 사람이 보고 실행하므로 이 제한은 MCP 에만 둔다.
func checkRedisArgs(args []string, maxRows int) error {
	cmd := strings.ToUpper(args[0])
	if msg, ok := redisDenied[cmd]; ok {
		return errors.New(msg)
	}
	scanCount := optRule{name: "COUNT", offset: 1, min: 1, max: maxRedisCount}
	switch cmd {
	case "SCAN":
		return checkOption(args[min(2, len(args)):], scanCount)
	case "SSCAN", "HSCAN", "ZSCAN":
		return checkOption(args[min(3, len(args)):], scanCount)
	case "XRANGE", "XREVRANGE":
		return checkOption(args[min(4, len(args)):], optRule{name: "COUNT", offset: 1, min: 1, max: int64(maxRows), required: true})
	case "MEMORY":
		if len(args) > 1 && strings.EqualFold(args[1], "USAGE") {
			return checkOption(args[min(3, len(args)):], optRule{name: "SAMPLES", offset: 1, min: 1, max: maxRedisCount})
		}
	case "ZRANGEBYSCORE":
		return checkOption(args[min(4, len(args)):], optRule{name: "LIMIT", offset: 2, min: 0, max: int64(maxRows), required: true})
	case "LRANGE", "ZRANGE", "ZREVRANGE":
		if len(args) < 4 {
			return fmt.Errorf("%s needs key, start and stop", cmd)
		}
		if cmd == "ZRANGE" {
			for _, a := range args[4:] {
				if strings.EqualFold(a, "BYSCORE") || strings.EqualFold(a, "BYLEX") {
					return checkOption(args[4:], optRule{name: "LIMIT", offset: 2, min: 0, max: int64(maxRows), required: true})
				}
			}
		}
		start, err1 := strconv.ParseInt(args[2], 10, 64)
		stop, err2 := strconv.ParseInt(args[3], 10, 64)
		// 부호가 다르면(0 -1 처럼) 길이에 따라 전체가 된다. 같은 부호끼리는 뺄셈이 넘치지 않는다
		if err1 != nil || err2 != nil || (start < 0) != (stop < 0) || stop-start >= int64(maxRows) {
			return fmt.Errorf("%s range must be integer indices of the same sign spanning at most %d elements", cmd, maxRows)
		}
	}
	return nil
}

// checkOption 은 opts 에 나오는 r.name 마다 값을 확인한다. Redis 는 같은 옵션이 반복되면 마지막 값을 쓰므로 전부 본다.
// 키·패턴이 옵션 이름과 같아도 값이 정수가 아니면 거부한다(안전 쪽).
func checkOption(opts []string, r optRule) error {
	found := false
	for i, a := range opts {
		if !strings.EqualFold(a, r.name) {
			continue
		}
		found = true
		if i+r.offset >= len(opts) {
			return fmt.Errorf("%s needs a value", r.name)
		}
		n, err := strconv.ParseInt(opts[i+r.offset], 10, 64)
		if err != nil || n < r.min || n > r.max {
			return fmt.Errorf("%s must be an integer between %d and %d", r.name, r.min, r.max)
		}
	}
	if r.required && !found {
		return fmt.Errorf("%s is required (at most %d) to bound the reply", r.name, r.max)
	}
	return nil
}
