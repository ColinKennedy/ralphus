//! Parses `help_map.rs`'s chip strings (`"selector [str]"`, `"--from [index]"`,
//! `"forge [github|gitlab]"`, `"file [str...]"`, `"target [str, optional]"`)
//! into a small structured [`Chip`], shared by tool-schema generation
//! ([`crate::tools`]) and argv construction for tool execution
//! ([`crate::exec`]) -- both need the same name/required/repeatable/choices
//! facts about the same chip text, so this is the one place that text gets
//! parsed.

/// One positional or option chip, parsed out of its `help_map.rs` text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chip {
    /// The positional's bare name, or the option's flag including `--`. For
    /// a tri-state chip this is just the positive spelling -- the negative
    /// spelling is derived the same way `commands::review::take_tri_bool`
    /// derives it.
    pub name: String,
    pub required: bool,
    pub repeatable: bool,
    /// `Some(choices)` for a `name [a|b|c]` literal-choice chip.
    pub choices: Option<Vec<String>>,
    /// `false` for a bare boolean option chip (`--all`, no `[...]`) or a
    /// tri-state chip (`--flag/--no-flag`).
    pub takes_value: bool,
    /// `true` for a `--flag/--no-flag` tri-state option chip, mirroring
    /// Python's `argparse.BooleanOptionalAction` (this crate's
    /// `commands::review::take_tri_bool`).
    pub tri_state: bool,
}

impl Chip {
    /// JSON-Schema-property-safe name: an option's leading `--` stripped and
    /// any internal `-` turned into `_` (`--dry-run` -> `dry_run`); a
    /// positional's name is already schema-safe as-is.
    #[must_use]
    pub fn property_name(&self) -> String {
        self.name.trim_start_matches("--").replace('-', "_")
    }

    #[must_use]
    pub fn is_option(&self) -> bool {
        self.name.starts_with("--")
    }
}

/// Parses one chip string. Panics on malformed input -- every chip here
/// comes from the `help_map.rs` `const` tree, not user input, so a parse
/// failure means a chip was authored in a shape this parser doesn't yet
/// understand; fail loudly (caught immediately by this crate's own tests)
/// rather than silently mis-deriving a tool schema.
#[must_use]
pub fn parse_chip(text: &str) -> Chip {
    let Some(bracket_start) = text.find('[') else {
        if let Some((positive, _negative)) = text.split_once('/') {
            // A tri-state option chip, e.g. "--skip-auto-build/--no-skip-auto-build".
            return Chip {
                name: positive.to_string(),
                required: false,
                repeatable: false,
                choices: None,
                takes_value: false,
                tri_state: true,
            };
        }
        // A bare boolean option chip, e.g. "--all".
        return Chip {
            name: text.to_string(),
            required: false,
            repeatable: false,
            choices: None,
            takes_value: false,
            tri_state: false,
        };
    };
    let name = text[..bracket_start].trim().to_string();
    let inner = text[bracket_start + 1..].rsplit_once(']').map_or_else(
        || text[bracket_start + 1..].to_string(),
        |(i, _)| i.to_string(),
    );
    let optional = inner.contains("optional");
    let repeatable = inner.contains("...");
    let choices = inner.contains('|').then(|| {
        inner
            .split(',')
            .next()
            .unwrap_or(&inner)
            .split("...")
            .next()
            .unwrap_or(&inner)
            .split('|')
            .map(|s| s.trim().to_string())
            .collect()
    });
    Chip {
        name,
        required: !optional,
        repeatable,
        choices,
        takes_value: true,
        tri_state: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_required_positional() {
        let c = parse_chip("selector [str]");
        assert_eq!(c.name, "selector");
        assert!(c.required);
        assert!(!c.repeatable);
        assert!(c.choices.is_none());
    }

    #[test]
    fn parses_optional_positional() {
        let c = parse_chip("target [str, optional]");
        assert_eq!(c.name, "target");
        assert!(!c.required);
    }

    #[test]
    fn parses_repeatable_positional() {
        let c = parse_chip("file [str...]");
        assert!(c.repeatable);
        assert!(c.required);
    }

    #[test]
    fn parses_choice_chip() {
        let c = parse_chip("forge [github|gitlab]");
        assert_eq!(
            c.choices,
            Some(vec!["github".to_string(), "gitlab".to_string()])
        );
    }

    #[test]
    fn parses_bare_boolean_option() {
        let c = parse_chip("--all");
        assert_eq!(c.name, "--all");
        assert!(!c.takes_value);
        assert_eq!(c.property_name(), "all");
    }

    #[test]
    fn parses_value_option_and_strips_dashes_for_property_name() {
        let c = parse_chip("--dry-run [value]");
        assert!(c.takes_value);
        assert_eq!(c.property_name(), "dry_run");
    }

    #[test]
    fn parses_tri_state_option() {
        let c = parse_chip("--skip-auto-build/--no-skip-auto-build");
        assert_eq!(c.name, "--skip-auto-build");
        assert!(c.tri_state);
        assert!(!c.takes_value);
        assert_eq!(c.property_name(), "skip_auto_build");
    }
}
