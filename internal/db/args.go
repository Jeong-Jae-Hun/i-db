package db

// Query 는 바인딩 인자가 있는 문장이다. Args 의 string 은 텍스트로 보내 서버가 자리에 맞는 타입으로 읽는다.
// ReadOnly 면 RunArgs 가 문장을 READ ONLY 트랜잭션 안에서 돌려, 첫 단어 판정이 놓친 쓰기(CTE 안 쓰기·select … into·nextval)를
// 서버가 거절하게 한다(잠긴 prod, R54).
type Query struct {
	SQL      string
	Args     []any
	ReadOnly bool
}
