package dynamo

import "testing"

func TestIsSelect_SELECT만_받고_나머지는_쓰기로_본다(t *testing.T) {
	cases := []struct {
		q    string
		want bool
	}{
		{`SELECT * FROM "t"`, true},
		{`  select a FROM t;`, true},
		{"-- 메모\n/* 여러\n줄 */ SeLeCt * FROM \"t\"", true},
		{`INSERT INTO "t" VALUE {'pk':'a'}`, false},
		{`UPDATE "t" SET a=1 WHERE pk='a'`, false},
		{`DELETE FROM "t" WHERE pk='a'`, false},
		{`EXISTS(SELECT * FROM "t" WHERE pk='a')`, false},
		{"-- SELECT\nDELETE FROM t", false},
		{"/* SELECT */ DELETE FROM t", false},
		{`SELECT * FROM t; DELETE FROM t`, false},
		{`'SELECT' FROM t`, false},
		{``, false},
		{`-- 주석만`, false},
	}
	for _, c := range cases {
		if got := isSelect(tokenize(c.q)); got != c.want {
			t.Errorf("isSelect(%q) = %v, want %v", c.q, got, c.want)
		}
	}
}

func TestTarget_테이블과_인덱스_이름을_읽는다(t *testing.T) {
	cases := []struct {
		q, table, index string
		ok              bool
	}{
		{`SELECT * FROM "orders" WHERE pk='a'`, "orders", "", true},
		{`SELECT * FROM orders`, "orders", "", true},
		{`SELECT * FROM "orders"."by-status" WHERE status='x'`, "orders", "by-status", true},
		{`SELECT * FROM "a""b"`, `a"b`, "", true},
		{`SELECT 'FROM x' FROM "real"`, "real", "", true},
		{`SELECT *`, "", "", false},
	}
	for _, c := range cases {
		table, index, ok := target(tokenize(c.q))
		if table != c.table || index != c.index || ok != c.ok {
			t.Errorf("target(%q) = %q %q %v, want %q %q %v", c.q, table, index, ok, c.table, c.index, c.ok)
		}
	}
}

func TestHasKeyCondition_파티션_키_조건이_있을_때만_스캔이_아니다(t *testing.T) {
	cases := []struct {
		name, q string
		want    bool
	}{
		{"따옴표 이름 =", `SELECT * FROM "t" WHERE "pk" = 'a'`, true},
		{"맨 이름 =", `SELECT * FROM t WHERE pk='a' AND sk > 3`, true},
		{"정렬 키 뒤에 와도", `SELECT * FROM t WHERE sk > 3 AND "pk" = 'a'`, true},
		{"값이 왼쪽", `SELECT * FROM t WHERE 'a' = pk`, true},
		{"IN", `SELECT * FROM t WHERE pk IN ['a','b']`, true},
		{"in 소문자", `SELECT * FROM t WHERE "pk" in ['a']`, true},
		{"괄호로 감싼 조건", `SELECT * FROM t WHERE (pk = 'a' AND sk > 1)`, true},
		{"괄호 조각 AND", `SELECT * FROM t WHERE (pk = 'a') AND sk > 1`, true},
		{"OR 양쪽 모두 키도 스캔으로 본다", `SELECT * FROM t WHERE pk = 'a' OR pk = 'b'`, false},
		{"괄호 안 OR 도", `SELECT * FROM t WHERE (pk = 'a' OR pk = 'b') AND sk > 1`, false},
		{"키 고정 뒤 다른 조건의 OR", `SELECT * FROM t WHERE pk = 'a' AND (sk = 1 OR sk = 2)`, true},
		{"끝 세미콜론", `SELECT * FROM t WHERE pk = 'a';`, true},
		{"ORDER BY", `SELECT * FROM t WHERE pk = 'a' ORDER BY sk DESC`, true},
		{"WHERE 없음", `SELECT * FROM t`, false},
		{"다른 속성만", `SELECT * FROM t WHERE status = 'x'`, false},
		{"OR 한쪽만 키", `SELECT * FROM t WHERE pk = 'a' OR status = 'x'`, false},
		{"부등호", `SELECT * FROM t WHERE pk > 'a'`, false},
		{"같지 않음", `SELECT * FROM t WHERE pk <> 'a'`, false},
		{"NOT IN", `SELECT * FROM t WHERE pk NOT IN ['a']`, false},
		{"NOT 앞", `SELECT * FROM t WHERE NOT pk = 'a'`, false},
		{"begins_with", `SELECT * FROM t WHERE begins_with(pk, 'a')`, false},
		{"함수 안 비교", `SELECT * FROM t WHERE size(pk = 'a')`, false},
		{"문자열 속 키 이름", `SELECT * FROM t WHERE status = 'pk = 1'`, false},
		{"주석 속 조건", "SELECT * FROM t WHERE status = 'x' -- pk = 'a'", false},
		{"중첩 경로", `SELECT * FROM t WHERE info.pk = 'a'`, false},
		{"대소문자 다른 이름", `SELECT * FROM t WHERE PK = 'a'`, false},
		{"속성끼리 비교", `SELECT * FROM t WHERE other = pk`, false},
		{"선택 열에만", `SELECT pk FROM t WHERE status = 'x'`, false},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			if got := hasKeyCondition(tokenize(c.q), "pk"); got != c.want {
				t.Errorf("hasKeyCondition(%q) = %v, want %v", c.q, got, c.want)
			}
		})
	}
}
