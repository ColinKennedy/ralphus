//! Resolving a review's exhibit settings ([`crate::exhibits::ExhibitSettings`]).
//!
//! Each setting layers per-review override > database-backed project
//! settings > the project's `.ralphus.toml [review]` value > the live global
//! config > the built-in default. That chain is resolved when the review is
//! read (`GuardianView::effective_exhibit*`), so this module only converts
//! the resolved view fields into the shape `crate::exhibits` consumes.

use crate::exhibits::ExhibitSettings;

/// The exhibit settings in force for `guardian`, from its `effective_*`
/// fields. Unknown kind names are dropped and an empty list falls back to
/// the default order, so the result always names at least one visual kind.
#[must_use]
pub fn resolve(
    _store: &crate::store_lock::StoreHandle,
    guardian: &crate::guardian::GuardianView,
) -> ExhibitSettings {
    from_view(guardian)
}

/// [`resolve`] without a store handle.
#[must_use]
pub fn from_view(guardian: &crate::guardian::GuardianView) -> ExhibitSettings {
    ExhibitSettings {
        enabled: guardian.effective_exhibits_enabled,
        max_per_pr: (guardian.effective_exhibits_max_per_pr as usize).max(1),
        infer: guardian.effective_infer_exhibits,
        kind_preference: crate::config::exhibit_kinds_or_default(Some(
            &guardian.effective_exhibit_kind_preference,
        )),
        max_video_seconds: guardian.effective_exhibit_max_video_seconds.max(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exhibits::ExhibitKind;
    use crate::store::{ProjectReviewSettings, Store};

    #[test]
    fn defaults_when_nothing_is_set() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(from_view(&g), ExhibitSettings::default());
    }

    #[test]
    fn review_override_beats_project_settings_and_both_resolve_live() {
        let store = Store::open_in_memory().unwrap();
        store.register_project("p", "", "/repo", "git").unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store
            .set_project_review_settings(
                "p",
                &ProjectReviewSettings {
                    exhibits_enabled: Some(false),
                    exhibits_max_per_pr: Some(2),
                    exhibit_kind_preference: Some(vec!["screenshot".into()]),
                    ..ProjectReviewSettings::default()
                },
            )
            .unwrap();
        let s = from_view(&store.get_guardian(&id).unwrap());
        assert!(!s.enabled);
        assert_eq!(s.max_per_pr, 2);
        assert_eq!(s.kind_preference, vec![ExhibitKind::Screenshot]);

        store
            .set_guardian_exhibits_enabled(&id, Some(true))
            .unwrap();
        store
            .set_guardian_exhibit_kind_preference(&id, Some(&["video".into(), "gif".into()]))
            .unwrap();
        store
            .set_guardian_exhibit_max_video_seconds(&id, Some(30))
            .unwrap();
        store.set_guardian_infer_exhibits(&id, Some(false)).unwrap();
        let s = from_view(&store.get_guardian(&id).unwrap());
        assert!(s.enabled);
        assert_eq!(s.max_per_pr, 2);
        assert!(!s.infer);
        assert_eq!(
            s.kind_preference,
            vec![ExhibitKind::Video, ExhibitKind::Gif]
        );
        assert_eq!(s.max_video_seconds, 30);

        store
            .set_guardian_exhibits_max_per_pr(&id, Some(5))
            .unwrap();
        assert_eq!(from_view(&store.get_guardian(&id).unwrap()).max_per_pr, 5);
        store.set_guardian_exhibits_max_per_pr(&id, None).unwrap();
        store
            .set_guardian_exhibit_kind_preference(&id, None)
            .unwrap();
        let s = from_view(&store.get_guardian(&id).unwrap());
        assert_eq!(s.max_per_pr, 2);
        assert_eq!(s.kind_preference, vec![ExhibitKind::Screenshot]);
    }
}
