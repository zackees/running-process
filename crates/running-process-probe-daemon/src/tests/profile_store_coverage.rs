use super::*;
use crate::profile::session::ResolvedSample;
use crate::profile::symbolize::Frame;

fn result(name: &str) -> SessionResult {
    SessionResult {
        samples: vec![ResolvedSample {
            os_tid: 1,
            frames: vec![
                Frame {
                    function: name.into(),
                    module: "fixture".into(),
                    relative_address: 1,
                },
                Frame {
                    function: "root".into(),
                    module: "fixture".into(),
                    relative_address: 2,
                },
            ],
            truncated: false,
        }],
        ..Default::default()
    }
}

#[test]
fn store_assigns_ids_returns_clones_and_evicts_oldest() {
    let store = ProfileStore::new();
    assert!(store.is_empty());
    for index in 0..(MAX_PROFILES + 3) {
        assert_eq!(
            store.insert(result(&format!("leaf-{index}"))),
            index as u64 + 1
        );
    }
    assert_eq!(store.len(), MAX_PROFILES);
    assert_eq!(store.ids(), (4..=11).rev().collect::<Vec<_>>());
    assert!(store.get(1).is_none());
    assert_eq!(
        store.get(11).unwrap().samples[0].frames[0].function,
        "leaf-10"
    );
}

#[test]
fn eviction_drops_expired_entries_even_below_count_limit() {
    let stored = Instant::now();
    let mut entries = HashMap::from([
        (
            1,
            Entry {
                result: result("expired"),
                stored,
            },
        ),
        (
            2,
            Entry {
                result: result("fresh"),
                stored: stored + PROFILE_TTL,
            },
        ),
    ]);
    evict_at(&mut entries, stored + PROFILE_TTL + Duration::from_secs(1));
    assert_eq!(entries.len(), 1);
    assert!(entries.contains_key(&2));
}

#[test]
fn collapsed_tree_skips_bad_rows_merges_prefixes_and_orders_hot_first() {
    let tree = collapsed_to_tree(
        "root;cold 1\nroot;hot 9\nroot;hot 2\nmissing-count\nroot;bad nope\n;; 3\n",
    );
    assert_eq!(tree.value, 15);
    assert_eq!(tree.children[0].name, "root");
    assert_eq!(tree.children[0].value, 12);
    assert_eq!(tree.children[0].children[0].name, "hot");
    assert_eq!(tree.children[0].children[0].value, 11);
}

#[test]
fn session_tree_merges_identical_stacks_and_sorts_siblings() {
    let mut session = result("hot");
    session.samples.extend([
        result("hot").samples.remove(0),
        result("cold").samples.remove(0),
    ]);
    let tree = session_to_tree(&session);
    assert_eq!(tree.value, 3);
    assert_eq!(tree.children[0].name, "root");
    assert_eq!(tree.children[0].children[0].name, "hot");
    assert_eq!(tree.children[0].children[0].value, 2);
}
