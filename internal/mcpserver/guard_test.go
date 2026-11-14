package mcpserver

import "testing"

func TestCheckRedisArgs_정보_노출과_상한_없는_읽기를_거부한다(t *testing.T) {
	rejected := [][]string{
		{"CLIENT", "LIST"}, {"slowlog", "get"}, {"MONITOR"}, {"KEYS", "*"},
		{"SMEMBERS", "s"}, {"HGETALL", "h"}, {"hkeys", "h"}, {"HVALS", "h"},
		{"LRANGE", "l", "0", "-1"}, {"LRANGE", "l", "0", "200"}, {"LRANGE", "l", "a", "b"}, {"LRANGE", "l"},
		{"ZRANGE", "z", "0", "-1"}, {"ZREVRANGE", "z", "-1000", "-1"}, {"LRANGE", "l", "0", "9223372036854775807"},
		{"ZRANGE", "z", "-inf", "+inf", "BYSCORE"}, {"ZRANGE", "z", "-inf", "+inf", "BYSCORE", "LIMIT", "0", "-1"},
		{"ZRANGEBYSCORE", "z", "-inf", "+inf"},
		{"XRANGE", "x", "-", "+"}, {"XREVRANGE", "x", "+", "-", "COUNT", "100000"},
		{"SCAN", "0", "COUNT", "100000"}, {"HSCAN", "h", "0", "COUNT", "10", "COUNT", "5000"}, {"SSCAN", "s", "0", "COUNT"},
		{"MEMORY", "USAGE", "k", "SAMPLES", "0"},
	}
	for _, args := range rejected {
		if err := checkRedisArgs(args, 200); err == nil {
			t.Errorf("거부해야 한다: %v", args)
		}
	}
	allowed := [][]string{
		{"GET", "k"}, {"LRANGE", "l", "0", "199"}, {"LRANGE", "l", "-10", "-1"}, {"ZRANGE", "z", "0", "9", "WITHSCORES"},
		{"ZRANGE", "z", "-inf", "+inf", "BYSCORE", "LIMIT", "0", "50"}, {"ZRANGEBYSCORE", "z", "0", "10", "LIMIT", "5", "10"},
		{"XRANGE", "x", "-", "+", "COUNT", "10"}, {"SCAN", "0", "MATCH", "a:*", "COUNT", "1000"}, {"ZSCAN", "z", "0"},
		{"MEMORY", "USAGE", "k"}, {"MEMORY", "USAGE", "k", "SAMPLES", "5"}, {"MEMORY", "STATS"},
	}
	for _, args := range allowed {
		if err := checkRedisArgs(args, 200); err != nil {
			t.Errorf("통과해야 한다: %v: %v", args, err)
		}
	}
}
