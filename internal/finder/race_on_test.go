//go:build race

package finder

// raceOn 은 -race 빌드에서 참이다. 속도 측정 테스트는 경합 검출기가 실행을 몇 배 늦춰 건너뛴다.
const raceOn = true
