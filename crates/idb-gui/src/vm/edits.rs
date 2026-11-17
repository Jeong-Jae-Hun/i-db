//! 결과 표 칸 편집(R15.2). 바꿈은 원본 행 자리에 붙여 두어 정렬·거르기로 순서가 바뀌어도 그 행을 따라간다.
//! 추가 행은 원본 뒤 자리(rows.len()+k)를 쓴다. 제출 묶음 규칙은 Go `internal/grid/edit.go` 의 changeSet 과 같다.

use std::collections::BTreeMap;

use idb_engine::{Change, ChangeOp, ChangeSet, Origin};

use super::grid::NULL;

/// 이보다 긴 칸은 엔진이 잘라 보낸 것이라 원래 값과 같을 수 없다(64KiB)
const MAX_CELL_BYTES: usize = 64 << 10;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RowEdit {
    /// 열 → 새 값(None 은 NULL)
    vals: BTreeMap<usize, Option<String>>,
    deleted: bool,
    inserted: bool,
}

pub struct Edits {
    pub origin: Origin,
    /// 결과 i 열의 PK 순서 자리
    pk_cols: Vec<usize>,
    base_rows: usize,
    rows: BTreeMap<usize, RowEdit>,
    inserts: usize,
    /// 되돌리기용: 바꾸기 전 그 행의 상태
    log: Vec<(usize, Option<RowEdit>)>,
}

impl Edits {
    /// origin 의 PK 열이 결과에 다 있어야 편집할 수 있다(엔진이 그럴 때만 origin 을 준다).
    pub fn new(origin: Origin, base_rows: usize) -> Option<Edits> {
        let pk_cols = origin
            .pk
            .iter()
            .map(|k| origin.cols.iter().position(|c| c == k))
            .collect::<Option<Vec<usize>>>()?;
        Some(Edits {
            origin,
            pk_cols,
            base_rows,
            rows: BTreeMap::new(),
            inserts: 0,
            log: Vec::new(),
        })
    }

    pub fn pending(&self) -> usize {
        self.rows.len()
    }

    pub fn inserted_rows(&self) -> usize {
        self.inserts
    }

    /// 식·생성 열과 GENERATED ALWAYS IDENTITY 열은 고칠 수 없다(INSERT 값으로도 못 쓴다).
    pub fn editable(&self, col: usize) -> bool {
        self.origin
            .cols
            .get(col)
            .is_some_and(|c| !c.is_empty() && !self.origin.always.contains(c))
    }

    /// 칸에 보일 값이다. 바꾸지 않았으면 None.
    pub fn value(&self, row: usize, col: usize) -> Option<Option<&str>> {
        self.rows.get(&row)?.vals.get(&col).map(|v| v.as_deref())
    }

    pub fn is_deleted(&self, row: usize) -> bool {
        self.rows.get(&row).is_some_and(|r| r.deleted)
    }

    pub fn is_inserted(&self, row: usize) -> bool {
        row >= self.base_rows
    }

    fn remember(&mut self, row: usize) {
        self.log.push((row, self.rows.get(&row).cloned()));
    }

    /// 칸 하나를 바꾼다. base 는 그 행의 원래 칸 글이다(추가 행은 없음). 원래 값으로 돌아오면 바꿈에서 뺀다.
    pub fn set(&mut self, row: usize, col: usize, v: Option<String>, base: Option<&str>) -> Result<(), String> {
        if !self.editable(col) {
            return Err(format!("{} 열은 고칠 수 없다", self.origin.cols.get(col).map_or("?", |s| s)));
        }
        if base.is_some_and(|b| b.len() > MAX_CELL_BYTES) {
            return Err("잘린 칸은 고칠 수 없다".into());
        }
        self.remember(row);
        let inserted = self.is_inserted(row);
        let e = self.rows.entry(row).or_default();
        e.inserted = inserted;
        let same = match (&v, base) {
            (None, Some(b)) => b == NULL,
            // 결과는 NULL 을 "NULL" 글자로 싣는다. 원래 값이 NULL 이면 글자 "NULL" 입력은 바꿈이다
            (Some(s), Some(b)) => s == b && b != NULL,
            _ => false,
        };
        if same && !inserted {
            e.vals.remove(&col);
        } else {
            e.vals.insert(col, v);
        }
        if e.vals.is_empty() && !e.deleted && !e.inserted {
            self.rows.remove(&row);
        }
        Ok(())
    }

    /// 지울 행으로 표시하거나 푼다. 추가 행은 그냥 뺀다.
    pub fn toggle_delete(&mut self, row: usize) {
        self.remember(row);
        if self.is_inserted(row) {
            self.rows.remove(&row);
            return;
        }
        let e = self.rows.entry(row).or_default();
        e.deleted = !e.deleted;
        if e.vals.is_empty() && !e.deleted {
            self.rows.remove(&row);
        }
    }

    /// 빈 추가 행을 만들고 그 자리를 돌려준다. 값을 정하지 않은 열은 INSERT 에서 빠져 DB 기본값을 받는다.
    pub fn add_row(&mut self) -> usize {
        let row = self.base_rows + self.inserts;
        self.remember(row);
        self.inserts += 1;
        self.rows.insert(
            row,
            RowEdit {
                inserted: true,
                ..Default::default()
            },
        );
        row
    }

    /// 마지막 바꿈 하나를 되돌린다.
    pub fn undo(&mut self) -> bool {
        let Some((row, before)) = self.log.pop() else {
            return false;
        };
        match before {
            Some(e) => {
                self.rows.insert(row, e);
            }
            None => {
                self.rows.remove(&row);
                if self.is_inserted(row) && row + 1 == self.base_rows + self.inserts {
                    self.inserts -= 1;
                }
            }
        }
        true
    }

    pub fn clear(&mut self) {
        self.rows.clear();
        self.log.clear();
        self.inserts = 0;
    }

    /// 제출 묶음이다. rows 는 원본 결과 행이다. 같은 유일 키를 지우고 다시 넣어도 부딪히지 않게 삭제·수정·추가 순이다.
    pub fn change_set(&self, rows: &[Vec<String>]) -> ChangeSet {
        let o = &self.origin;
        let mut cs = ChangeSet {
            table: o.table.clone(),
            pk: o.pk.clone(),
            changes: Vec::new(),
            override_system: false,
        };
        for op in [ChangeOp::Delete, ChangeOp::Update, ChangeOp::Insert] {
            for (&i, e) in &self.rows {
                let this = if e.inserted {
                    ChangeOp::Insert
                } else if e.deleted {
                    ChangeOp::Delete
                } else {
                    ChangeOp::Update
                };
                if this != op {
                    continue;
                }
                let mut ch = Change {
                    op,
                    key: BTreeMap::new(),
                    values: BTreeMap::new(),
                    old: BTreeMap::new(),
                };
                if !e.inserted {
                    let row = &rows[i];
                    for (k, name) in o.pk.iter().enumerate() {
                        ch.key.insert(name.clone(), row[self.pk_cols[k]].clone());
                    }
                    // 옛값: 수정은 바꾸는 열, 삭제는 PK 가 아닌 모든 테이블 열. 그새 바뀐 행을 고치지 않게 엔진이 WHERE 로 확인한다
                    for (c, name) in o.cols.iter().enumerate() {
                        let edited = e.vals.contains_key(&c);
                        if name.is_empty() || (!e.deleted && !edited) || (e.deleted && o.pk.contains(name)) {
                            continue;
                        }
                        let v = &row[c];
                        if v.len() > MAX_CELL_BYTES {
                            continue;
                        }
                        ch.old.insert(name.clone(), (v != NULL).then(|| v.clone()));
                    }
                }
                if !e.deleted {
                    for (&c, v) in &e.vals {
                        ch.values.insert(o.cols[c].clone(), v.clone());
                    }
                }
                // 값을 하나도 안 정한 추가 행은 보내지 않는다(엔진이 빈 INSERT 를 거절한다)
                if op == ChangeOp::Insert && ch.values.is_empty() {
                    continue;
                }
                cs.changes.push(ch);
            }
        }
        cs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use idb_engine::TableRef;

    fn origin() -> Origin {
        Origin {
            table: TableRef {
                schema: "public".into(),
                table: "users".into(),
            },
            pk: vec!["id".into()],
            cols: vec!["id".into(), "name".into(), "".into()],
            always: vec![],
        }
    }

    fn rows() -> Vec<Vec<String>> {
        vec![
            vec!["1".into(), "kim".into(), "x".into()],
            vec!["2".into(), "NULL".into(), "y".into()],
        ]
    }

    #[test]
    fn PK_열이_결과에_없으면_편집할_수_없다() {
        let mut o = origin();
        o.pk = vec!["uid".into()];
        assert!(Edits::new(o, 2).is_none());
    }

    #[test]
    fn 식_열은_못_고치고_원래_값으로_돌리면_바꿈에서_빠진다() {
        let mut e = Edits::new(origin(), 2).unwrap();
        assert!(e.set(0, 2, Some("z".into()), Some("x")).is_err());
        e.set(0, 1, Some("lee".into()), Some("kim")).unwrap();
        assert_eq!(e.pending(), 1);
        assert_eq!(e.value(0, 1), Some(Some("lee")));
        e.set(0, 1, Some("kim".into()), Some("kim")).unwrap();
        assert_eq!(e.pending(), 0);
    }

    #[test]
    fn NULL_칸에_글자_NULL_을_넣으면_바꿈이고_NULL_은_아니다() {
        let mut e = Edits::new(origin(), 2).unwrap();
        e.set(1, 1, None, Some("NULL")).unwrap();
        assert_eq!(e.pending(), 0);
        e.set(1, 1, Some("NULL".into()), Some("NULL")).unwrap();
        assert_eq!(e.pending(), 1);
    }

    #[test]
    fn 제출_묶음은_삭제_수정_추가_순이고_옛값을_싣는다() {
        let mut e = Edits::new(origin(), 2).unwrap();
        let r = e.add_row();
        e.set(r, 1, Some("new".into()), None).unwrap();
        e.set(1, 1, Some("park".into()), Some("NULL")).unwrap();
        e.toggle_delete(0);
        let cs = e.change_set(&rows());
        let ops: Vec<ChangeOp> = cs.changes.iter().map(|c| c.op).collect();
        assert_eq!(ops, vec![ChangeOp::Delete, ChangeOp::Update, ChangeOp::Insert]);

        let del = &cs.changes[0];
        assert_eq!(del.key, BTreeMap::from([("id".to_string(), "1".to_string())]));
        // 삭제 옛값은 PK 가 아닌 테이블 열 전부(식 열 제외)
        assert_eq!(del.old, BTreeMap::from([("name".to_string(), Some("kim".to_string()))]));
        assert!(del.values.is_empty());

        let upd = &cs.changes[1];
        assert_eq!(upd.old, BTreeMap::from([("name".to_string(), None)]));
        assert_eq!(upd.values, BTreeMap::from([("name".to_string(), Some("park".to_string()))]));

        let ins = &cs.changes[2];
        assert!(ins.key.is_empty() && ins.old.is_empty());
        assert_eq!(ins.values, BTreeMap::from([("name".to_string(), Some("new".to_string()))]));
        assert_eq!(cs.table.table, "users");
        assert_eq!(cs.pk, vec!["id"]);
    }

    #[test]
    fn 값_없는_추가_행은_보내지_않는다() {
        let mut e = Edits::new(origin(), 2).unwrap();
        e.add_row();
        assert_eq!(e.pending(), 1);
        assert!(e.change_set(&rows()).changes.is_empty());
    }

    #[test]
    fn 되돌리기는_한_단계씩_앞_상태로_간다() {
        let mut e = Edits::new(origin(), 2).unwrap();
        e.set(0, 1, Some("a".into()), Some("kim")).unwrap();
        e.set(0, 1, Some("b".into()), Some("kim")).unwrap();
        let r = e.add_row();
        assert_eq!(e.inserted_rows(), 1);
        assert!(e.undo());
        assert_eq!(e.inserted_rows(), 0);
        assert!(!e.is_inserted(r) || e.value(r, 1).is_none());
        assert!(e.undo());
        assert_eq!(e.value(0, 1), Some(Some("a")));
        assert!(e.undo());
        assert_eq!(e.pending(), 0);
        assert!(!e.undo());
    }

    #[test]
    fn 잘린_칸은_고칠_수_없다() {
        let mut e = Edits::new(origin(), 2).unwrap();
        let long = "a".repeat(MAX_CELL_BYTES + 10);
        assert!(e.set(0, 1, Some("b".into()), Some(&long)).is_err());
    }

    #[test]
    fn 지운_행을_다시_누르면_풀린다() {
        let mut e = Edits::new(origin(), 2).unwrap();
        e.toggle_delete(1);
        assert!(e.is_deleted(1));
        e.toggle_delete(1);
        assert_eq!(e.pending(), 0);
    }
}
