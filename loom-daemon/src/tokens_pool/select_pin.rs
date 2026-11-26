//! Account-pinned selection (issue #14981, follow-up to #14371 / PR #14596).
//!
//! A Claude step run inside a persistent seat writes its transcript into that
//! seat's `CLAUDE_CONFIG_DIR`, so a later `--resume` only works when the launch
//! lands in the **same account**. [`select_pinned_token`] is the selector's
//! answer for that launch: it never draws, it either hands out the named
//! account or refuses.
//!
//! Fail closed: an account that is missing, empty, not a Claude account,
//! bad-marked (for the model class, when one is given) or ranked
//! `exhausted`/`blocked` is an error. It is never replaced by another account,
//! because another account cannot read the transcript.
//!
//! A child module of [`super`] (declared there with `#[path]`) so it reuses
//! the selector's private eligibility helpers instead of copying them.

use super::{
    is_bad_for_class, is_non_claude, list_token_files, ranking_hard_exclusions,
    read_manifest_index, read_token_file, resolve_tokens_dir, stem, upstream_id_for,
    EmptyTokenPoolError, SelectedToken,
};
use std::path::Path;

/// `true` for a name that can only be a plain pool account name: letters,
/// digits, `.`, `_`, `-`, not starting with `.` (so no path component, no
/// dotfile such as `.ranking`).
fn is_plain_account_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Select exactly the account `pin`, or fail.
///
/// `model_class` narrows the `.bad_tokens` check the same way it does for
/// [`super::select_token_for_class_and_role`]: an account bad-marked only for
/// another model class is still eligible.
///
/// # Errors
/// [`EmptyTokenPoolError`] naming why `pin` cannot be honoured.
pub fn select_pinned_token(
    workspace: &Path,
    pin: &str,
    model_class: Option<&str>,
) -> Result<SelectedToken, EmptyTokenPoolError> {
    let refuse = |why: String| {
        EmptyTokenPoolError(format!(
            "Pinned account '{pin}' cannot be used: {why}. A session resume needs this exact \
             account; refusing to select a different one."
        ))
    };
    if !is_plain_account_name(pin) {
        return Err(refuse("it is not a valid account name".to_string()));
    }
    let tokens_dir = resolve_tokens_dir(workspace);
    let Some(file) = list_token_files(&tokens_dir)
        .into_iter()
        .find(|p| stem(p) == pin)
    else {
        return Err(refuse(format!("no {pin}.token in {}", tokens_dir.display())));
    };
    let manifest = read_manifest_index(&tokens_dir);
    if is_non_claude(&manifest, pin) {
        return Err(refuse("it is not a Claude account".to_string()));
    }
    if is_bad_for_class(workspace, pin, model_class) {
        return Err(refuse("it is marked bad".to_string()));
    }
    let ranking_file = tokens_dir.join(".ranking");
    if let Some(status) = ranking_hard_exclusions(&tokens_dir, &ranking_file).get(pin) {
        return Err(refuse(format!("the ranking reports it {status}")));
    }
    let key = read_token_file(&file)
        .map_err(|e| refuse(format!("its token file is unreadable ({e})")))?;
    if key.is_empty() {
        return Err(refuse("its token file is empty".to_string()));
    }
    Ok(SelectedToken {
        name: pin.to_string(),
        file,
        key,
        mode: "pinned",
        upstream_id: upstream_id_for(&manifest, pin),
    })
}

#[cfg(test)]
#[path = "select_pin_tests.rs"]
mod tests;
