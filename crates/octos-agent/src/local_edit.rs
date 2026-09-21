//! H05 local-edit experiment policy and shared prompt guidance.

use std::sync::OnceLock;

pub(crate) const LOCAL_EDIT_ENV: &str = "OCTOS_LOCAL_EDIT";
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
const DIFF_EDIT_DESCRIPTION: &str = "Apply one unified diff to an existing file. Prefer one \
multi-hunk call for multiple separated changes in that file; use edit_file for one contiguous \
change and write_file for new files or intentional whole-file rewrites. Context matching allows \
a +-3-line offset.";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LocalEditPolicy {
    pub enabled: bool,
}

impl LocalEditPolicy {
    pub(crate) fn parse(value: Option<&str>) -> Self {
        let enabled = match value.map(str::trim) {
            Some("1") => true,
            Some(value)
                if value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("on") =>
            {
                true
            }
            None | Some("0") => false,
            Some(value)
                if value.eq_ignore_ascii_case("false") || value.eq_ignore_ascii_case("off") =>
            {
                false
            }
            Some(_) => {
                tracing::warn!("unknown OCTOS_LOCAL_EDIT value; local-edit guidance disabled");
                eprintln!("warning: unknown OCTOS_LOCAL_EDIT value; local-edit guidance disabled");
                false
            }
        };
        Self { enabled }
    }

    pub(crate) fn from_env() -> Self {
        static POLICY: OnceLock<LocalEditPolicy> = OnceLock::new();
        *POLICY.get_or_init(|| Self::parse(std::env::var(LOCAL_EDIT_ENV).ok().as_deref()))
    }
}

pub(crate) fn append_guidance(prompt: &mut String, enabled: bool) {
    if enabled && !prompt.contains(LOCAL_EDIT_GUIDANCE_HEADING) {
        prompt.push_str(LOCAL_EDIT_GUIDANCE);
    }
}

pub(crate) fn tool_description<'a>(name: &str, fallback: &'a str, enabled: bool) -> &'a str {
    if !enabled {
        return fallback;
    }
    match name {
        "write_file" => WRITE_FILE_DESCRIPTION,
        "edit_file" => EDIT_FILE_DESCRIPTION,
        "diff_edit" => DIFF_EDIT_DESCRIPTION,
        _ => fallback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_defaults_and_unknown_values_are_off() {
        assert!(!LocalEditPolicy::parse(None).enabled);
        assert!(!LocalEditPolicy::parse(Some("0")).enabled);
        assert!(!LocalEditPolicy::parse(Some("false")).enabled);
        assert!(!LocalEditPolicy::parse(Some("off")).enabled);
        assert!(!LocalEditPolicy::parse(Some("unexpected")).enabled);
    }

    #[test]
    fn policy_accepts_documented_enabled_values() {
        assert!(LocalEditPolicy::parse(Some("1")).enabled);
        assert!(LocalEditPolicy::parse(Some("true")).enabled);
        assert!(LocalEditPolicy::parse(Some("TRUE")).enabled);
        assert!(LocalEditPolicy::parse(Some(" on ")).enabled);
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
        assert_eq!(tool_description("write_file", fallback, false), fallback);
        assert!(tool_description("write_file", fallback, true).contains("new file"));
        assert!(tool_description("edit_file", fallback, true).contains("contiguous"));
        assert!(tool_description("diff_edit", fallback, true).contains("multi-hunk"));
        assert_eq!(tool_description("read_file", fallback, true), fallback);
    }
}
