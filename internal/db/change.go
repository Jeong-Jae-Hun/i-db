package db

// ─── 결과 표 편집 계약 (v3) ───
// grid 가 편집 묶음을 만들고, db 가 한 트랜잭션으로 제출한다. 두 쪽이 같이 쓰는 타입만 둔다.

const (
	ChangeUpdate ChangeOp = iota
	ChangeInsert
	ChangeDelete
)

type ChangeOp int

// Origin 은 결과의 출처 테이블이다. Cols[i] 는 결과 i 번째 열의 테이블 열 이름이고, 식 열이면 "" 다.
// 생성 열과 PK 가 아닌 GENERATED ALWAYS IDENTITY 열도 값을 쓸 수 없어 "" 다.
// Always 는 GENERATED ALWAYS IDENTITY 인 PK 열이다. 행을 찾는 데는 쓰지만 INSERT 값으로는 쓸 수 없다.
type Origin struct {
	Table  TableRef
	PK     []string
	Cols   []string
	Always []string
}

// Change 는 행 하나의 바꿈이다. Key 는 PK 열 → 원래 값(update·delete), Values 는 열 → 새 값이고 nil 은 NULL 이다.
// Old 는 읽었을 때의 값(update 는 바꾸는 열, delete 는 PK 가 아닌 열)이다. 행이 그새 바뀌었거나
// 다른 행을 가리키면 조건이 맞지 않아 0행이 되고 묶음 전체가 되돌려진다.
type Change struct {
	Op     ChangeOp
	Key    map[string]string
	Values map[string]*string
	Old    map[string]*string
}

type ChangeSet struct {
	Table   TableRef
	PK      []string
	Changes []Change
	// Override 는 INSERT 에 OVERRIDING SYSTEM VALUE 를 붙인다. 되돌리기가 지운 행을 GENERATED ALWAYS IDENTITY 키 그대로 되살릴 때 쓴다.
	// 이 INSERT 는 정확히 한 행을 넣어야 한다(ApplyChanges).
	Override bool
}

// Applied 는 ApplyChanges 가 받은 RETURNING * 행이다. Rows[i] 는 cs.Changes[i] 의 반영 뒤 행(UPDATE·INSERT)이나
// 지운 행(DELETE)으로, 열 이름 → 출력 함수 텍스트(자르지 않음)이고 nil 은 NULL 이다.
// Generated 는 묶음에 DELETE 가 있을 때만 채우는 생성 열 이름이다. 되살리는 INSERT 에서 뺀다.
// NoReturning 은 RETURNING * 가 권한으로 거절돼 그것 없이 반영했다는 뜻이다. Rows 가 비어 되돌릴 수 없다.
type Applied struct {
	Rows        []map[string]*string
	Generated   []string
	NoReturning bool
}
