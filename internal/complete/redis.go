package complete

import (
	"strings"
	"unicode"
)

// keyMode 는 명령의 몇 번째 인자가 키인지다. 인자 번호는 명령 이름 다음부터 1이다.
const (
	keyNone     keyMode = iota
	keyFirst            // 1
	keyAll              // 1, 2, 3, ...
	keyFirstTwo         // 1, 2
	keyOdd              // 1, 3, 5, ... (MSET 키 값 키 값)
	keySecond           // 2 (MEMORY USAGE 키)
)

var redisCommands = []redisCmd{
	{"GET", "key", keyFirst, nil}, {"SET", "key value [EX seconds] [NX|XX]", keyFirst, nil},
	{"DEL", "key [key ...]", keyAll, nil}, {"UNLINK", "key [key ...]", keyAll, nil},
	{"EXISTS", "key [key ...]", keyAll, nil}, {"TYPE", "key", keyFirst, nil}, {"TTL", "key", keyFirst, nil},
	{"PTTL", "key", keyFirst, nil}, {"EXPIRE", "key seconds", keyFirst, nil}, {"PEXPIRE", "key ms", keyFirst, nil},
	{"EXPIREAT", "key unix-time", keyFirst, nil}, {"PERSIST", "key", keyFirst, nil},
	{"RENAME", "key newkey", keyFirstTwo, nil}, {"RENAMENX", "key newkey", keyFirstTwo, nil},
	{"COPY", "source destination [REPLACE]", keyFirstTwo, nil}, {"DUMP", "key", keyFirst, nil},
	{"TOUCH", "key [key ...]", keyAll, nil}, {"KEYS", "pattern", keyNone, nil},
	{"SCAN", "cursor [MATCH pattern] [COUNT n] [TYPE type]", keyNone, nil},
	{"RANDOMKEY", "", keyNone, nil}, {"DBSIZE", "", keyNone, nil},
	{"MGET", "key [key ...]", keyAll, nil}, {"MSET", "key value [key value ...]", keyOdd, nil},
	{"MSETNX", "key value [key value ...]", keyOdd, nil}, {"SETNX", "key value", keyFirst, nil},
	{"SETEX", "key seconds value", keyFirst, nil}, {"GETSET", "key value", keyFirst, nil},
	{"GETDEL", "key", keyFirst, nil}, {"GETEX", "key [EX seconds|PERSIST]", keyFirst, nil},
	{"APPEND", "key value", keyFirst, nil}, {"STRLEN", "key", keyFirst, nil},
	{"GETRANGE", "key start end", keyFirst, nil}, {"SETRANGE", "key offset value", keyFirst, nil},
	{"INCR", "key", keyFirst, nil}, {"INCRBY", "key increment", keyFirst, nil},
	{"INCRBYFLOAT", "key increment", keyFirst, nil}, {"DECR", "key", keyFirst, nil},
	{"DECRBY", "key decrement", keyFirst, nil}, {"GETBIT", "key offset", keyFirst, nil},
	{"SETBIT", "key offset value", keyFirst, nil}, {"BITCOUNT", "key [start end]", keyFirst, nil},
	{"HGET", "key field", keyFirst, nil}, {"HSET", "key field value [field value ...]", keyFirst, nil},
	{"HSETNX", "key field value", keyFirst, nil}, {"HMGET", "key field [field ...]", keyFirst, nil},
	{"HGETALL", "key", keyFirst, nil}, {"HDEL", "key field [field ...]", keyFirst, nil},
	{"HEXISTS", "key field", keyFirst, nil}, {"HKEYS", "key", keyFirst, nil}, {"HVALS", "key", keyFirst, nil},
	{"HLEN", "key", keyFirst, nil}, {"HINCRBY", "key field increment", keyFirst, nil},
	{"HSCAN", "key cursor [MATCH pattern] [COUNT n]", keyFirst, nil},
	{"LPUSH", "key element [element ...]", keyFirst, nil}, {"RPUSH", "key element [element ...]", keyFirst, nil},
	{"LPOP", "key [count]", keyFirst, nil}, {"RPOP", "key [count]", keyFirst, nil},
	{"LRANGE", "key start stop", keyFirst, nil}, {"LLEN", "key", keyFirst, nil},
	{"LINDEX", "key index", keyFirst, nil}, {"LSET", "key index element", keyFirst, nil},
	{"LREM", "key count element", keyFirst, nil}, {"LTRIM", "key start stop", keyFirst, nil},
	{"LMOVE", "source destination LEFT|RIGHT LEFT|RIGHT", keyFirstTwo, nil},
	{"RPOPLPUSH", "source destination", keyFirstTwo, nil},
	{"SADD", "key member [member ...]", keyFirst, nil}, {"SREM", "key member [member ...]", keyFirst, nil},
	{"SMEMBERS", "key", keyFirst, nil}, {"SISMEMBER", "key member", keyFirst, nil},
	{"SCARD", "key", keyFirst, nil}, {"SPOP", "key [count]", keyFirst, nil},
	{"SRANDMEMBER", "key [count]", keyFirst, nil}, {"SSCAN", "key cursor [MATCH pattern] [COUNT n]", keyFirst, nil},
	{"SINTER", "key [key ...]", keyAll, nil}, {"SUNION", "key [key ...]", keyAll, nil},
	{"SDIFF", "key [key ...]", keyAll, nil}, {"SMOVE", "source destination member", keyFirstTwo, nil},
	{"ZADD", "key score member [score member ...]", keyFirst, nil}, {"ZREM", "key member [member ...]", keyFirst, nil},
	{"ZRANGE", "key start stop [BYSCORE|BYLEX] [REV] [WITHSCORES]", keyFirst, nil},
	{"ZRANGEBYSCORE", "key min max [WITHSCORES]", keyFirst, nil},
	{"ZREVRANGE", "key start stop [WITHSCORES]", keyFirst, nil}, {"ZCARD", "key", keyFirst, nil},
	{"ZSCORE", "key member", keyFirst, nil}, {"ZRANK", "key member", keyFirst, nil},
	{"ZINCRBY", "key increment member", keyFirst, nil}, {"ZCOUNT", "key min max", keyFirst, nil},
	{"ZSCAN", "key cursor [MATCH pattern] [COUNT n]", keyFirst, nil},
	{"XADD", "key id field value [field value ...]", keyFirst, nil}, {"XRANGE", "key start end [COUNT n]", keyFirst, nil},
	{"XREVRANGE", "key end start [COUNT n]", keyFirst, nil}, {"XLEN", "key", keyFirst, nil},
	{"XDEL", "key id [id ...]", keyFirst, nil}, {"XTRIM", "key MAXLEN n", keyFirst, nil},
	{"PFADD", "key element [element ...]", keyFirst, nil}, {"PFCOUNT", "key [key ...]", keyAll, nil},
	{"GEOADD", "key lng lat member", keyFirst, nil}, {"GEOPOS", "key member [member ...]", keyFirst, nil},
	{"PUBLISH", "channel message", keyNone, nil}, {"SUBSCRIBE", "channel [channel ...]", keyNone, nil},
	{"WATCH", "key [key ...]", keyAll, nil}, {"MULTI", "", keyNone, nil}, {"EXEC", "", keyNone, nil},
	{"DISCARD", "", keyNone, nil}, {"SELECT", "index", keyNone, nil}, {"PING", "[message]", keyNone, nil},
	{"ECHO", "message", keyNone, nil}, {"INFO", "[section]", keyNone, nil}, {"TIME", "", keyNone, nil},
	{"EVAL", "script numkeys [key ...] [arg ...]", keyNone, nil},
	{"EVALSHA", "sha1 numkeys [key ...] [arg ...]", keyNone, nil},
	{"FLUSHDB", "[ASYNC|SYNC]", keyNone, nil}, {"FLUSHALL", "[ASYNC|SYNC]", keyNone, nil},
	{"CLIENT", "subcommand", keyNone, []string{"LIST", "INFO", "GETNAME", "SETNAME", "ID", "KILL", "PAUSE", "UNPAUSE"}},
	{"CONFIG", "subcommand", keyNone, []string{"GET", "SET", "RESETSTAT", "REWRITE"}},
	{"SLOWLOG", "subcommand", keyNone, []string{"GET", "LEN", "RESET"}},
	{"MEMORY", "subcommand", keySecond, []string{"USAGE", "STATS", "DOCTOR", "PURGE"}},
	{"OBJECT", "subcommand key", keySecond, []string{"ENCODING", "FREQ", "IDLETIME", "REFCOUNT"}},
	{"XINFO", "subcommand key", keySecond, []string{"STREAM", "GROUPS", "CONSUMERS"}},
	{"SCRIPT", "subcommand", keyNone, []string{"LOAD", "EXISTS", "FLUSH", "KILL"}},
	{"CLUSTER", "subcommand", keyNone, []string{"INFO", "NODES", "SLOTS", "SHARDS", "KEYSLOT"}},
	{"COMMAND", "[subcommand]", keyNone, []string{"COUNT", "DOCS", "INFO", "LIST"}},
	{"ACL", "subcommand", keyNone, []string{"WHOAMI", "LIST", "USERS", "CAT"}},
	{"PUBSUB", "subcommand", keyNone, []string{"CHANNELS", "NUMSUB", "NUMPAT"}},
}

type keyMode uint8

type redisCmd struct {
	name, hint string
	keys       keyMode
	subs       []string
}

// SuggestRedis 는 커서가 있는 줄의 첫 낱말이면 명령을, 키 자리면 keys 에서 고른 키를 제안한다.
// 인자는 db.SplitArgs 와 같은 규칙(공백으로 나누고 따옴표 안 공백은 유지, 따옴표 안 \x 는 x)으로 센다.
func SuggestRedis(q Query, keys []string) []Item {
	off := clampOffset(q)
	line := q.Text[strings.LastIndexByte(q.Text[:off], '\n')+1 : off]

	var args []string
	var cur strings.Builder
	inArg, escaped, argStart := false, false, 0
	var quote rune
	for i, c := range line {
		if !inArg && !unicode.IsSpace(c) {
			argStart = i
		}
		switch {
		case escaped:
			cur.WriteRune(c)
			escaped = false
		case quote != 0 && c == '\\':
			escaped = true
		case quote != 0 && c == quote:
			quote = 0
		case quote != 0:
			cur.WriteRune(c)
		case c == '"' || c == '\'':
			quote, inArg = c, true
		case unicode.IsSpace(c):
			if inArg {
				args = append(args, cur.String())
				cur.Reset()
				inArg = false
			}
		default:
			cur.WriteRune(c)
			inArg = true
		}
	}
	prefix := cur.String()
	w := typed{name: prefix}
	if inArg {
		w.size = len(line) - argStart
	}
	pos := len(args) // 커서가 있는 인자 번호. 0 이면 명령 이름
	if pos == 0 {
		items := make([]Item, 0, len(redisCommands))
		for _, c := range redisCommands {
			items = append(items, Item{Label: c.name, Kind: KindCommand, Detail: c.hint, Insert: caseLike(c.name, prefix)})
		}
		return rank(items, w)
	}

	var cmd *redisCmd
	for i := range redisCommands {
		if strings.EqualFold(redisCommands[i].name, args[0]) {
			cmd = &redisCommands[i]
		}
	}
	if cmd == nil {
		return nil
	}
	if pos == 1 && len(cmd.subs) > 0 {
		items := make([]Item, 0, len(cmd.subs))
		for _, s := range cmd.subs {
			items = append(items, Item{Label: s, Kind: KindCommand, Insert: caseLike(s, prefix)})
		}
		return rank(items, w)
	}
	isKey := cmd.keys == keyAll || cmd.keys == keyFirst && pos == 1 || cmd.keys == keyFirstTwo && pos <= 2 ||
		cmd.keys == keyOdd && pos%2 == 1 || cmd.keys == keySecond && pos == 2
	if !isKey {
		return nil
	}
	items := make([]Item, 0, len(keys))
	for _, k := range keys {
		items = append(items, Item{Label: k, Kind: KindKey, Insert: quoteArg(k)})
	}
	return rank(items, w)
}

// quoteArg 는 공백·따옴표·백슬래시가 있는 키를 db.SplitArgs 가 그대로 되살리도록 따옴표로 감싼다.
func quoteArg(s string) string {
	if s != "" && !strings.ContainsAny(s, "\"'\\") && !strings.ContainsFunc(s, unicode.IsSpace) {
		return s
	}
	return `"` + strings.NewReplacer(`\`, `\\`, `"`, `\"`).Replace(s) + `"`
}
