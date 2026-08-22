//! A function (not a `const`) so any future test needing to force the other
//! branch has a single named seam to work with.

#[must_use]
pub fn is_windows() -> bool {
    cfg!(target_os = "windows")
}
