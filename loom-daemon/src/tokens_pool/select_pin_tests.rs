//! Unit tests for [`super`] — account-pinned selection (#14981).

use super::super::select_token;
use super::super::tests::{make_pool, pool_dir};
use super::*;
use serial_test::serial;
use std::fs;

fn isolate() {
    std::env::set_var(super::super::super::paths::SHARED_TOKENS_DIR_ENV, "");
}

fn restore() {
    std::env::remove_var(super::super::super::paths::SHARED_TOKENS_DIR_ENV);
}

#[test]
#[serial]
fn a_pinned_account_is_returned_even_when_others_exist() {
    isolate();
    let tmp = make_pool(&["alpha", "beta", "gamma"]);
    for _ in 0..8 {
        let sel = select_pinned_token(tmp.path(), "beta", None).unwrap();
        assert_eq!((sel.name.as_str(), sel.mode, sel.key.as_str()), ("beta", "pinned", "key-beta"));
    }
    restore();
}

#[test]
#[serial]
fn a_missing_account_fails_closed_instead_of_drawing_another() {
    isolate();
    let tmp = make_pool(&["alpha", "beta"]);
    let err = select_pinned_token(tmp.path(), "zeta", None).unwrap_err();
    assert!(
        err.0.contains("Pinned account 'zeta'") && err.0.contains("no zeta.token"),
        "{err}"
    );
    restore();
}

#[test]
#[serial]
fn a_bad_marked_account_fails_closed() {
    isolate();
    let tmp = make_pool(&["alpha", "beta"]);
    super::super::super::bad_tokens::mark_bad(tmp.path(), "beta", "auth_401").unwrap();
    let err = select_pinned_token(tmp.path(), "beta", None).unwrap_err();
    assert!(err.0.contains("marked bad"), "{err}");
    // The unpinned selector still has an answer: the pin is what refuses.
    assert_eq!(select_token(tmp.path(), None).unwrap().name, "alpha");
    restore();
}

#[test]
#[serial]
fn an_exhausted_ranking_row_fails_closed() {
    isolate();
    let tmp = make_pool(&["alpha", "beta"]);
    fs::write(
        pool_dir(tmp.path()).join(".ranking"),
        "alpha|available|0.1\nbeta|exhausted|1.0\n",
    )
    .unwrap();
    let err = select_pinned_token(tmp.path(), "beta", None).unwrap_err();
    assert!(err.0.contains("exhausted"), "{err}");
    assert_eq!(select_pinned_token(tmp.path(), "alpha", None).unwrap().name, "alpha");
    restore();
}

#[test]
#[serial]
fn an_empty_token_file_fails_closed() {
    isolate();
    let tmp = make_pool(&["alpha"]);
    fs::write(pool_dir(tmp.path()).join("alpha.token"), "\n").unwrap();
    let err = select_pinned_token(tmp.path(), "alpha", None).unwrap_err();
    assert!(err.0.contains("empty"), "{err}");
    restore();
}

#[test]
#[serial]
fn a_name_that_is_not_a_plain_account_name_is_refused() {
    isolate();
    let tmp = make_pool(&["alpha"]);
    for bad in ["", "../alpha", "a/b", ".ranking", "al pha"] {
        let err = select_pinned_token(tmp.path(), bad, None).unwrap_err();
        assert!(err.0.contains("not a valid account name"), "{bad}: {err}");
    }
    restore();
}
