// Package plantree 는 EXPLAIN (FORMAT JSON) 결과를 나무로 읽고, 노드마다 자기 시간·비율을 셈해 펼쳐 보여 준다.
package plantree

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
)

// 실제 행이 예상의 10배 넘게 많거나 적으면 통계가 틀린 것으로 본다
const misestimateRatio = 10

type Buffers struct {
	SharedHit     int64 `json:"Shared Hit Blocks"`
	SharedRead    int64 `json:"Shared Read Blocks"`
	SharedDirtied int64 `json:"Shared Dirtied Blocks"`
	SharedWritten int64 `json:"Shared Written Blocks"`
	TempRead      int64 `json:"Temp Read Blocks"`
	TempWritten   int64 `json:"Temp Written Blocks"`
}

// Node 의 json 태그 필드는 EXPLAIN JSON 키 그대로다. 행 수는 PostgreSQL 18 부터 소수(루프 평균)로 나와 float64 로 받는다.
// ActualTime·ActualRows 는 루프 한 번 평균이다.
type Node struct {
	Type            string  `json:"Node Type"`
	Relation        string  `json:"Relation Name"`
	Alias           string  `json:"Alias"`
	Index           string  `json:"Index Name"`
	JoinType        string  `json:"Join Type"`
	Parent          string  `json:"Parent Relationship"` // Outer·Inner·InitPlan·SubPlan 등
	SubplanName     string  `json:"Subplan Name"`
	CTEName         string  `json:"CTE Name"`
	StartupCost     float64 `json:"Startup Cost"`
	TotalCost       float64 `json:"Total Cost"`
	PlanRows        float64 `json:"Plan Rows"`
	PlanWidth       int     `json:"Plan Width"`
	ActualStart     float64 `json:"Actual Startup Time"`
	ActualTime      float64 `json:"Actual Total Time"`
	ActualRows      float64 `json:"Actual Rows"`
	Loops           float64 `json:"Actual Loops"`
	WorkersPlanned  int     `json:"Workers Planned"`
	WorkersLaunched int     `json:"Workers Launched"`
	Buffers
	Filter          string   `json:"Filter"`
	JoinFilter      string   `json:"Join Filter"`
	IndexCond       string   `json:"Index Cond"`
	RecheckCond     string   `json:"Recheck Cond"`
	HashCond        string   `json:"Hash Cond"`
	MergeCond       string   `json:"Merge Cond"`
	SortKey         []string `json:"Sort Key"`
	GroupKey        []string `json:"Group Key"`
	RemovedByFilter float64  `json:"Rows Removed by Filter"`
	RemovedByJoin   float64  `json:"Rows Removed by Join Filter"`
	RemovedRecheck  float64  `json:"Rows Removed by Index Recheck"`
	Children        []*Node  `json:"Plans"`

	// 아래는 Parse 가 채운다
	Analyzed      bool    `json:"-"`
	PlanningTime  float64 `json:"-"` // 뿌리 노드에만
	ExecutionTime float64 `json:"-"` // 뿌리 노드에만
	TotalTime     float64 `json:"-"` // 루프를 모두 더한 시간(ms). 병렬 구간은 프로세스 수로 나눠 벽시계 시간에 맞춘다
	SelfTime      float64 `json:"-"` // TotalTime 에서 자식 TotalTime 을 뺀 값(0 이상)
	SelfCost      float64 `json:"-"` // TotalCost 에서 자식 TotalCost 를 뺀 값(0 이상)
	Percent       float64 `json:"-"` // 전체에서 자기 몫(ANALYZE 면 시간, 아니면 비용) 0~100
}

type explainDoc struct {
	Plan          *Node    `json:"Plan"`
	PlanningTime  float64  `json:"Planning Time"`
	ExecutionTime *float64 `json:"Execution Time"`
}

// Parse 는 EXPLAIN (FORMAT JSON[, ANALYZE, BUFFERS]) 출력(객체 하나를 담은 배열)을 읽는다. 배열 없이 객체만 와도 받는다.
func Parse(jsonText string) (*Node, error) {
	raw := bytes.TrimSpace([]byte(jsonText))
	var docs []explainDoc
	if len(raw) > 0 && raw[0] == '{' {
		docs = make([]explainDoc, 1)
		if err := json.Unmarshal(raw, &docs[0]); err != nil {
			return nil, fmt.Errorf("parse explain JSON: %w", err)
		}
	} else if err := json.Unmarshal(raw, &docs); err != nil {
		return nil, fmt.Errorf("parse explain JSON: %w", err)
	}
	if len(docs) == 0 || docs[0].Plan == nil || docs[0].Plan.Type == "" {
		return nil, errors.New("explain JSON has no plan")
	}
	d := docs[0]
	root := d.Plan
	root.PlanningTime = d.PlanningTime
	// ANALYZE 출력에만 Execution Time 이 있다. never executed 노드도 Actual Loops 0 으로 나와 키 유무로 가르지 않는다
	analyzed := d.ExecutionTime != nil
	if analyzed {
		root.ExecutionTime = *d.ExecutionTime
	}
	derive(root, 1)
	total := root.TotalCost
	if analyzed {
		total = root.TotalTime
	}
	nodes := flatten(root)
	// CTE Scan 의 시간에는 CTE 를 처음 계산한 시간이 들어 있다. 그 몫은 CTE InitPlan 노드가 이미 가지므로 뺀다.
	// CTE 를 여러 번 읽으면 첫 읽기에만 들어가지만 어느 것인지 알 수 없어 모두에서 빼고 0에서 자른다
	cteTime := map[string]float64{}
	for _, n := range nodes {
		if n.Parent == "InitPlan" && strings.HasPrefix(n.SubplanName, "CTE ") {
			cteTime[strings.TrimPrefix(n.SubplanName, "CTE ")] = n.TotalTime
		}
	}
	for _, n := range nodes {
		n.Analyzed = analyzed
		if n.Type == "CTE Scan" {
			n.SelfTime = max(0, n.SelfTime-cteTime[n.CTEName])
		}
		// 시간·비용이 무한대로 넘치면 Inf/Inf 가 NaN 이 되므로 양수일 때만 싣는다(NaN 비교는 거짓)
		if p := 100 * n.Self() / total; total > 0 && p > 0 {
			n.Percent = min(100, p)
		}
	}
	return root, nil
}

// Self 는 비싼 노드를 고르는 기준이다. ANALYZE 면 자기 시간, 아니면 자기 비용이다.
func (n *Node) Self() float64 {
	if n.Analyzed {
		return n.SelfTime
	}
	return n.SelfCost
}

// Misestimate 는 실제 행/예상 행 비다. 1보다 크면 예상보다 많다. 0행은 1행으로 보고, 실행하지 않은 노드는 0이다.
func (n *Node) Misestimate() float64 {
	if !n.Analyzed || n.Loops == 0 {
		return 0
	}
	return max(n.ActualRows, 1) / max(n.PlanRows, 1)
}

// Misestimated 는 실제 행이 예상과 misestimateRatio 배 이상 차이 나는지다.
func (n *Node) Misestimated() bool {
	r := n.Misestimate()
	return r >= misestimateRatio || (r > 0 && r <= 1.0/misestimateRatio)
}

// MisestimateMark 는 추정 오차 표시다. 예: 예상보다 12배 많으면 "↑12×", 6675배 적으면 "↓6675×". 오차가 작으면 빈 글이다.
func (n *Node) MisestimateMark() string {
	if !n.Misestimated() {
		return ""
	}
	if e := n.Misestimate(); e >= 1 {
		return fmt.Sprintf("↑%.0f×", e)
	}
	return fmt.Sprintf("↓%.0f×", 1/n.Misestimate())
}

// derive 는 TotalTime·SelfTime·SelfCost 를 채운다. div 는 이 노드를 돌린 프로세스 수다.
// Gather 아래 노드는 Actual Loops 가 워커+리더 수라서 그대로 곱하면 프로세스 시간의 합이 되므로 나눈다.
// CTE·InitPlan 은 그 결과를 읽는 노드(CTE Scan)의 시간에도 들어가 두 번 빠지므로 자기 시간은 0에서 자른다.
func derive(n *Node, div float64) {
	n.TotalTime = n.ActualTime * n.Loops / div
	childDiv := div
	if n.WorkersLaunched > 0 {
		childDiv = float64(n.WorkersLaunched + 1)
	}
	var childTime, childCost float64
	for _, c := range n.Children {
		derive(c, childDiv)
		childTime += c.TotalTime
		childCost += c.TotalCost
	}
	n.SelfTime = max(0, n.TotalTime-childTime)
	n.SelfCost = max(0, n.TotalCost-childCost)
}

// flatten 은 전위 순회 순서의 노드 목록이다.
func flatten(n *Node) []*Node {
	out := []*Node{n}
	for _, c := range n.Children {
		out = append(out, flatten(c)...)
	}
	return out
}
