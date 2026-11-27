use super::*;

/// 防御分支（正常 BFS 不可达）：parent 链在中间节点断裂必须返回 Ok(None)，
/// 让 path_with_max_results 降级为 found=false，而不是 found=true 配空 path。
#[test]
fn reconstruct_path_broken_parent_chain_returns_none() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let mut parent: HashMap<String, (String, String)> = HashMap::new();
    // c 的父是 b，但 b 没有父记录 → 回溯到 b 时链断裂（src=a 永不可达）。
    parent.insert("c".to_string(), ("b".to_string(), "r".to_string()));
    assert!(reconstruct_path(db.conn(), "a", "c", &parent)
        .unwrap()
        .is_none());
}

/// 完整链：2*length+1 个交替元素，端点取 entities 的 name（不是 canonical）。
#[test]
fn reconstruct_path_complete_chain_alternates() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    db.conn()
        .execute_batch(
            "INSERT INTO entities (canonical, name, entity_type, first_seen, last_seen)
             VALUES ('a', 'A', 'unknown', 0, 0), ('b', 'B', 'unknown', 0, 0);",
        )
        .unwrap();
    let mut parent: HashMap<String, (String, String)> = HashMap::new();
    parent.insert("b".to_string(), ("a".to_string(), "knows".to_string()));

    let steps = reconstruct_path(db.conn(), "a", "b", &parent)
        .unwrap()
        .unwrap();
    assert_eq!(steps.len(), 3);
    assert_eq!(
        steps[0],
        PathStep::Entity {
            canonical: "a".to_string(),
            name: "A".to_string()
        }
    );
    assert_eq!(
        steps[1],
        PathStep::Edge {
            rel_type: "knows".to_string()
        }
    );
    assert_eq!(
        steps[2],
        PathStep::Entity {
            canonical: "b".to_string(),
            name: "B".to_string()
        }
    );
}

/// M8c: 实体行缺失（并发删除）→ 名字回退 canonical 字面，路径仍完整。
#[test]
fn reconstruct_path_missing_entity_falls_back_to_canonical() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    db.conn()
        .execute_batch(
            "INSERT INTO entities (canonical, name, entity_type, first_seen, last_seen)
             VALUES ('a', 'A', 'unknown', 0, 0);",
        )
        .unwrap();
    let mut parent: HashMap<String, (String, String)> = HashMap::new();
    parent.insert("b".to_string(), ("a".to_string(), "knows".to_string()));

    let steps = reconstruct_path(db.conn(), "a", "b", &parent)
        .unwrap()
        .unwrap();
    assert_eq!(
        steps[2],
        PathStep::Entity {
            canonical: "b".to_string(),
            name: "b".to_string()
        },
        "missing entity row must fall back to the canonical literal"
    );
}

/// M8c: 名字查询的 SQLite 错误必须上抛（不得吞错伪装成 canonical 回退）。
/// progress handler 中断每个 VDBE 步：prepare 成功但查询中途报错。
#[test]
fn reconstruct_path_query_error_propagates() {
    let mut db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let mut parent: HashMap<String, (String, String)> = HashMap::new();
    parent.insert("b".to_string(), ("a".to_string(), "knows".to_string()));

    db.conn_mut().progress_handler(1, Some(|| true)).unwrap();
    let res = reconstruct_path(db.conn(), "a", "b", &parent);
    db.conn_mut()
        .progress_handler(0, None::<fn() -> bool>)
        .unwrap();
    assert!(res.is_err(), "SQLite failure must propagate, not degrade");
}
