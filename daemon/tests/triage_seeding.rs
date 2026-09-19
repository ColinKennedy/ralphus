//! Regression tests for RAL-467: seed default triage types on first run.
//!
//! These tests verify that:
//! - Fresh daemon instances automatically seed three default triage types
//!   (bug, feature, investigation) in addition to the built-in unclassified type
//! - Restarting against an existing config does not re-add removed types
//! - unclassified is read-only and permanent

mod common;

use ralphus_daemon::store::Store;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

fn temp_db(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ralphus-triage-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir.join("tasks.db")
}

#[test]
fn fresh_store_seeds_default_triage_types() {
    let store = Store::open_in_memory().expect("open store");

    let types = store.list_triage_types().expect("list types");
    let names: Vec<&str> = types.iter().map(|t| t.name.as_str()).collect();

    // All four types should be present
    assert!(names.contains(&"unclassified"), "unclassified missing");
    assert!(names.contains(&"bug"), "bug missing");
    assert!(names.contains(&"feature"), "feature missing");
    assert!(names.contains(&"investigation"), "investigation missing");

    // Verify exact count
    assert_eq!(names.len(), 4, "unexpected extra types: {:?}", names);

    // Verify descriptions match the expected values
    let types_map: std::collections::HashMap<&str, &str> = types
        .iter()
        .map(|t| (t.name.as_str(), t.description.as_str()))
        .collect();

    assert!(types_map["bug"].contains("error") || types_map["bug"].contains("flaw"));
    assert!(
        types_map["feature"].contains("capability") || types_map["feature"].contains("improvement")
    );
    assert!(
        types_map["investigation"].contains("research")
            || types_map["investigation"].contains("thought")
    );
}

#[test]
fn unclassified_type_cannot_be_removed() {
    let store = Store::open_in_memory().expect("open store");

    use ralphus_daemon::triage::DeregisterOutcome;
    let outcome = store
        .deregister_triage_type("unclassified")
        .expect("deregister attempt");

    assert_eq!(
        outcome,
        DeregisterOutcome::BuiltIn,
        "unclassified should not be removable"
    );

    // Verify it's still there
    let types = store.list_triage_types().expect("list types");
    let has_unclassified = types.iter().any(|t| t.name == "unclassified");
    assert!(has_unclassified, "unclassified was removed");
}

#[test]
fn unclassified_case_insensitive_protection() {
    let store = Store::open_in_memory().expect("open store");

    use ralphus_daemon::triage::DeregisterOutcome;

    // Try various case variations
    for variant in &["UNCLASSIFIED", "Unclassified", "UnClassified"] {
        let outcome = store
            .deregister_triage_type(variant)
            .expect("deregister attempt");
        assert_eq!(
            outcome,
            DeregisterOutcome::BuiltIn,
            "unclassified protection should be case-insensitive ({variant})"
        );
    }
}

#[test]
fn default_types_can_be_removed() {
    let store = Store::open_in_memory().expect("open store");

    use ralphus_daemon::triage::DeregisterOutcome;

    // Remove the bug type
    let outcome = store.deregister_triage_type("bug").expect("deregister");
    assert_eq!(
        outcome,
        DeregisterOutcome::Removed,
        "bug should be removable"
    );

    // Verify it's gone
    let types = store.list_triage_types().expect("list types");
    let has_bug = types.iter().any(|t| t.name == "bug");
    assert!(!has_bug, "bug type was not removed");

    // But other types should still exist
    let names: Vec<&str> = types.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"unclassified"));
    assert!(names.contains(&"feature"));
    assert!(names.contains(&"investigation"));
}

#[test]
fn file_based_store_does_not_reseed_after_removal() {
    let db_path = temp_db("reseed");

    // First open: create schema and seed defaults
    {
        let store = Store::open(&db_path).expect("open store");
        let types = store.list_triage_types().expect("list types");
        let names: Vec<&str> = types.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"bug"), "bug should exist initially");
        assert_eq!(names.len(), 4, "should have 4 types initially");

        // Remove the bug type
        use ralphus_daemon::triage::DeregisterOutcome;
        let outcome = store.deregister_triage_type("bug").expect("deregister bug");
        assert_eq!(outcome, DeregisterOutcome::Removed);
    }

    // Second open: verify that bug is NOT re-added
    {
        let store = Store::open(&db_path).expect("reopen store");
        let types = store.list_triage_types().expect("list types");
        let names: Vec<&str> = types.iter().map(|t| t.name.as_str()).collect();

        // bug should NOT be present after restart
        assert!(!names.contains(&"bug"), "removed bug type was re-seeded");

        // unclassified should still be there (always re-inserted)
        assert!(
            names.contains(&"unclassified"),
            "unclassified was not re-inserted"
        );

        // other defaults should still be there (never removed)
        assert!(names.contains(&"feature"));
        assert!(names.contains(&"investigation"));
    }
}

#[test]
fn default_types_are_editable() {
    let store = Store::open_in_memory().expect("open store");

    // Verify we can update a default type's description
    store
        .register_triage_type("bug", "BugReport", "Updated description for bugs")
        .expect("register/update type");

    let updated = store
        .get_triage_type("bug")
        .expect("get type")
        .expect("bug should exist");

    assert_eq!(updated.label, "BugReport");
    assert_eq!(updated.description, "Updated description for bugs");
}

#[test]
fn types_list_is_alphabetically_sorted() {
    let store = Store::open_in_memory().expect("open store");

    let types = store.list_triage_types().expect("list types");
    let names: Vec<&str> = types.iter().map(|t| t.name.as_str()).collect();

    // Verify alphabetical order
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "types should be in alphabetical order");
}
