//! H05 local-edit experiment policy and shared prompt guidance.

use std::sync::OnceLock;

pub(crate) const LOCAL_EDIT_ENV: &str = "OCTOS_LOCAL_EDIT";
pub(crate) const LOCAL_EDIT_STRICT_MATCH_ENV: &str = "OCTOS_LOCAL_EDIT_STRICT_MATCH";
pub(crate) const LOCAL_EDIT_GUIDANCE_HEADING: &str = "## File editing";
pub(crate) const LOCAL_EDIT_GUIDANCE: &str = "\n\n\
## File editing\n\n\
Choose before generating arguments:\n\
- New file: `write_file`.\n\
- One contiguous change in an existing file: `edit_file`.\n\
- Multiple separated changes in one existing file: one multi-hunk `diff_edit`.\n\
Use `write_file` on an existing file only when its complete current contents \
are visible and a whole-file rewrite is necessary. Edit multiple files with \
separate calls; mutations run serially.";
const WRITE_FILE_DESCRIPTION: &str = "Create a new file, or overwrite an existing file only when \
its complete current contents are visible and a whole-file rewrite is necessary. For local \
changes, use edit_file or diff_edit.";
const EDIT_FILE_DESCRIPTION: &str = "Replace one unique contiguous span in an existing file. Use \
diff_edit for multiple separated changes in the same file and write_file for new files or \
intentional whole-file rewrites. An exact old_string match is preferred; the current fuzzy \
fallbacks remain enabled.";
const STRICT_EDIT_FILE_DESCRIPTION: &str = "Replace one unique contiguous span in an existing \
file. Automatic writes require an exact or CRLF/LF-equivalent old_string match; approximate \
whitespace, indentation, escape, or block matches are returned only as suggestions. Use \
diff_edit for multiple separated changes.";
const DIFF_EDIT_DESCRIPTION: &str = "Apply one unified diff to an existing file. Prefer one \
multi-hunk call for multiple separated changes in that file; use edit_file for one contiguous \
change and write_file for new files or intentional whole-file rewrites. Context matching allows \
a +-3-line offset.";
const STRICT_DIFF_EDIT_DESCRIPTION: &str = "Apply one unified diff to an existing file. Automatic \
writes require exact context lines, with CRLF/LF treated as equivalent; trailing-whitespace \
matches are returned only as suggestions. Prefer one multi-hunk call for separated changes.";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LocalEditPolicy {
    pub enabled: bool,
    pub strict_match: bool,
}

impl LocalEditPolicy {
    pub(crate) fn parse_with_strict(value: Option<&str>, strict_match: Option<&str>) -> Self {
        let enabled = parse_flag(value, LOCAL_EDIT_ENV, "local-edit guidance");
        let strict_match = enabled
            && parse_flag(
                strict_match,
                LOCAL_EDIT_STRICT_MATCH_ENV,
                "strict local-edit matching",
            );
        Self {
            enabled,
            strict_match,
        }
    }

    pub(crate) fn from_env() -> Self {
        static POLICY: OnceLock<LocalEditPolicy> = OnceLock::new();
        *POLICY.get_or_init(|| {
            Self::parse_with_strict(
                std::env::var(LOCAL_EDIT_ENV).ok().as_deref(),
                std::env::var(LOCAL_EDIT_STRICT_MATCH_ENV).ok().as_deref(),
            )
        })
    }
}

fn parse_flag(value: Option<&str>, env: &str, feature: &str) -> bool {
    match value.map(str::trim) {
        Some("1") => true,
        Some(value) if value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("on") => {
            true
        }
        None | Some("0") => false,
        Some(value) if value.eq_ignore_ascii_case("false") || value.eq_ignore_ascii_case("off") => {
            false
        }
        Some(_) => {
            tracing::warn!(env, "unknown feature flag value; {feature} disabled");
            eprintln!("warning: unknown {env} value; {feature} disabled");
            false
        }
    }
}

pub(crate) fn append_guidance(prompt: &mut String, enabled: bool) {
    if enabled && !prompt.contains(LOCAL_EDIT_GUIDANCE_HEADING) {
        prompt.push_str(LOCAL_EDIT_GUIDANCE);
    }
}

pub(crate) fn tool_description<'a>(
    name: &str,
    fallback: &'a str,
    guidance_enabled: bool,
    strict_match: bool,
) -> &'a str {
    match (name, guidance_enabled, strict_match) {
        ("edit_file", _, true) => STRICT_EDIT_FILE_DESCRIPTION,
        ("diff_edit", _, true) => STRICT_DIFF_EDIT_DESCRIPTION,
        ("write_file", true, _) => WRITE_FILE_DESCRIPTION,
        ("edit_file", true, false) => EDIT_FILE_DESCRIPTION,
        ("diff_edit", true, false) => DIFF_EDIT_DESCRIPTION,
        (_, _, _) => fallback,
    }
}

pub(crate) fn tool_input_schema(
    name: &str,
    mut schema: serde_json::Value,
    enabled: bool,
) -> serde_json::Value {
    if enabled
        && name == "edit_file"
        && let Some(properties) = schema
            .get_mut("properties")
            .and_then(serde_json::Value::as_object_mut)
    {
        properties.insert(
            "replace_all".to_string(),
            serde_json::json!({
                "type": "boolean",
                "default": false,
                "description": "Replace every non-overlapping exact or CRLF/LF-equivalent match. Use only when every occurrence should change."
            }),
        );
    }
    schema
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_defaults_and_unknown_values_are_off() {
        for value in [
            None,
            Some("0"),
            Some("false"),
            Some("off"),
            Some("unexpected"),
        ] {
            assert_eq!(
                LocalEditPolicy::parse_with_strict(value, None),
                LocalEditPolicy::default()
            );
        }
        assert_eq!(
            LocalEditPolicy::parse_with_strict(Some("0"), Some("1")),
            LocalEditPolicy::default()
        );
    }

    #[test]
    fn policy_accepts_documented_enabled_values() {
        for value in [Some("1"), Some("true"), Some("TRUE"), Some(" on ")] {
            assert_eq!(
                LocalEditPolicy::parse_with_strict(value, None),
                LocalEditPolicy {
                    enabled: true,
                    strict_match: false,
                }
            );
        }
        assert_eq!(
            LocalEditPolicy::parse_with_strict(Some("1"), Some("true")),
            LocalEditPolicy {
                enabled: true,
                strict_match: true,
            }
        );
        assert_eq!(
            LocalEditPolicy::parse_with_strict(Some("1"), Some("unexpected")),
            LocalEditPolicy {
                enabled: true,
                strict_match: false,
            }
        );
    }

    #[test]
    fn guidance_is_disabled_or_appended_once() {
        let mut off = "base".to_string();
        append_guidance(&mut off, false);
        assert_eq!(off, "base");

        let mut on = "base".to_string();
        append_guidance(&mut on, true);
        append_guidance(&mut on, true);
        assert_eq!(on.matches(LOCAL_EDIT_GUIDANCE_HEADING).count(), 1);
        assert!(on.contains("New file: `write_file`"));
        assert!(on.contains("One contiguous change"));
        assert!(on.contains("Multiple separated changes"));
        assert!(on.contains("complete current contents"));
    }

    #[test]
    fn tool_descriptions_change_only_for_the_three_editors() {
        let fallback = "unchanged";
        let off = LocalEditPolicy::default();
        let enabled = LocalEditPolicy {
            enabled: true,
            strict_match: false,
        };
        let strict = LocalEditPolicy {
            enabled: true,
            strict_match: true,
        };
        assert_eq!(
            tool_description("write_file", fallback, off.enabled, off.strict_match),
            fallback
        );
        assert!(
            tool_description(
                "write_file",
                fallback,
                enabled.enabled,
                enabled.strict_match
            )
            .contains("new file")
        );
        assert!(
            tool_description("edit_file", fallback, enabled.enabled, enabled.strict_match)
                .contains("contiguous")
        );
        assert!(
            tool_description("diff_edit", fallback, enabled.enabled, enabled.strict_match)
                .contains("multi-hunk")
        );
        assert!(
            tool_description("edit_file", fallback, strict.enabled, strict.strict_match)
                .contains("suggestions")
        );
        assert!(
            tool_description("diff_edit", fallback, strict.enabled, strict.strict_match)
                .contains("suggestions")
        );
        assert_eq!(
            tool_description("read_file", fallback, enabled.enabled, enabled.strict_match),
            fallback
        );
    }

    #[test]
    fn replace_all_schema_is_exposed_only_when_local_edit_is_enabled() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "old_string": {"type": "string"}
            }
        });
        let off = tool_input_schema("edit_file", schema.clone(), false);
        let on = tool_input_schema("edit_file", schema.clone(), true);
        let other = tool_input_schema("write_file", schema.clone(), true);

        assert_eq!(off, schema);
        assert_eq!(other, schema);
        assert_eq!(on["properties"]["replace_all"]["type"], "boolean");
        assert_eq!(on["properties"]["replace_all"]["default"], false);
    }
}
