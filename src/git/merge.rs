use std::collections::BTreeMap;

use super::oid::Oid;

pub type FileMap = BTreeMap<String, (u32, Oid)>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeAction {
    Take(u32, Oid),
    Delete,
    Conflict {
        ours: Option<(u32, Oid)>,
        theirs: Option<(u32, Oid)>,
    },
}

/// 三方合并。比较的是 `(mode, oid)`，内容相同则对象名相同。
pub fn merge_maps(
    base: &FileMap,
    ours: &FileMap,
    theirs: &FileMap,
) -> BTreeMap<String, MergeAction> {
    let mut paths = BTreeMap::<String, ()>::new();
    for key in base.keys().chain(ours.keys()).chain(theirs.keys()) {
        paths.insert(key.clone(), ());
    }
    let mut out = BTreeMap::new();
    for path in paths.into_keys() {
        let b = base.get(&path).copied();
        let o = ours.get(&path).copied();
        let t = theirs.get(&path).copied();
        let action = match (b, o, t) {
            (_, Some(o), Some(t)) if o == t => MergeAction::Take(o.0, o.1),
            (Some(b), Some(o), Some(t)) if b == o && b != t => MergeAction::Take(t.0, t.1),
            (Some(b), Some(o), Some(t)) if b == t && b != o => MergeAction::Take(o.0, o.1),
            (None, Some(o), None) => MergeAction::Take(o.0, o.1),
            (None, None, Some(t)) => MergeAction::Take(t.0, t.1),
            (Some(b), Some(o), None) if b == o => MergeAction::Delete,
            (Some(b), None, Some(t)) if b == t => MergeAction::Delete,
            (Some(_), None, None) => MergeAction::Delete,
            (None, None, None) => continue,
            (_, ours, theirs) => MergeAction::Conflict { ours, theirs },
        };
        out.insert(path, action);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(n: u8) -> (u32, Oid) {
        let mut bytes = [0u8; 20];
        bytes[19] = n;
        (0o100644, Oid(bytes))
    }

    #[test]
    fn takes_one_sided_edit() {
        let mut base = FileMap::new();
        base.insert("a".into(), file(1));
        let mut ours = base.clone();
        ours.insert("a".into(), file(2));
        let theirs = base.clone();
        let merged = merge_maps(&base, &ours, &theirs);
        assert_eq!(merged["a"], MergeAction::Take(file(2).0, file(2).1));
    }

    #[test]
    fn conflicts_when_both_change() {
        let mut base = FileMap::new();
        base.insert("a".into(), file(1));
        let mut ours = base.clone();
        ours.insert("a".into(), file(2));
        let mut theirs = base.clone();
        theirs.insert("a".into(), file(3));
        let merged = merge_maps(&base, &ours, &theirs);
        assert!(matches!(merged["a"], MergeAction::Conflict { .. }));
    }
}
