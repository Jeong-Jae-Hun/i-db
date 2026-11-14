package db

// numericTypes 는 숫자로 정렬·정렬 방향·집계·차트를 할 PostgreSQL 타입이다(결과 Types 의 typname).
// money 는 "$1,234.00" 처럼 통화 글자가 붙어 숫자로 읽히지 않으므로 뺀다.
var numericTypes = map[string]bool{"int2": true, "int4": true, "int8": true, "float4": true, "float8": true, "numeric": true, "oid": true}

// IsNumericType 은 결과 열 타입 이름이 숫자 타입인지다. grid 와 chart 가 같은 판정을 쓰게 여기 한 벌만 둔다.
func IsNumericType(typ string) bool {
	return numericTypes[typ]
}
