//! `loom-daemon forge <issue|pr> view <N> --json …` on a Gitea checkout
//! (#14939): the live gate read of the numbered Judge and Doctor paths.
//!
//! On GitHub `forge <entity> view` is the byte-identical `gh` passthrough.
//! On Gitea it declined (exit 3) and the caller's shell path is `gh`, which
//! cannot resolve the remote, so both roles refused with `forge_unreachable`
//! before their first decision (qualification #14850). This module serves the
//! numeric `--json` view from the Gitea API instead, through
//! [`crate::gitea_read`], and prints it in the shape `gh … view --json` does:
//! compact JSON with the requested keys, `--jq` applied.
//!
//! The read settles kind first. `pr view` of an issue number and `issue view`
//! of a pull request exit `1` and print nothing, as do an unknown number, a
//! missing credential and an unreadable answer, so no later read can mistake
//! one for a target. What it does answer is the row as the forge has it — a
//! `CLOSED` or `MERGED` state, a `loom:frozen` label — and the role's gate
//! refuses on that: the read informs the gate, it is never the gate.

use std::path::Path;

use serde_json::{json, Map, Value};

use crate::forge_cached_view::ViewQuery;
use crate::forge_cmd::{detect_forge, ForgeType};

/// Exit code signalling "this shape is not served; fall back to the caller's
/// shell path".
pub const DECLINED: i32 = crate::forge_cmd::EX_FORGE_DECLINED;

/// The `--json` fields served for `issue view`.
const ISSUE_FIELDS: &[&str] = &[
    "body",
    "closedAt",
    "createdAt",
    "labels",
    "number",
    "state",
    "title",
    "updatedAt",
    "url",
];

/// The `--json` fields served for `pr view`. `headRefOid` and `mergeable` are
/// what a Doctor's claim starts from; `id` and `author` have no Gitea
/// counterpart and are not served.
const PR_FIELDS: &[&str] = &[
    "baseRefName",
    "body",
    "closedAt",
    "createdAt",
    "headRefName",
    "headRefOid",
    "isDraft",
    "labels",
    "mergeable",
    "mergedAt",
    "number",
    "state",
    "title",
    "updatedAt",
    "url",
];

/// Whether [`handle`] owns `forge <entity> <args>`: a `view` on a Gitea
/// checkout. Everything else on Gitea keeps declining as before, and on
/// GitHub the passthrough stands.
pub fn serves(args: &[String]) -> bool {
    args.first().is_some_and(|a| a == "view") && detect_forge(None) == ForgeType::Gitea
}

/// Entry point from `forge_cmd::dispatch`. Serves to stdout and exits `0`;
/// exits `1` with nothing on stdout when the number cannot be read as this
/// entity; exits [`DECLINED`] for a shape this module does not serve. Never
/// returns.
pub fn handle(entity: &str, args: &[String]) -> ! {
    let Some(query) =
        fields(entity).and_then(|fields| crate::forge_cached_view::parse_query_for(args, fields))
    else {
        decline();
    };
    let cwd = std::env::current_dir().ok();
    let rendered = repo(cwd.as_deref(), &query).and_then(|repo| {
        render(entity, &query, &repo, &mut |path| {
            crate::gitea_read::read(cwd.as_deref().unwrap_or_else(|| Path::new(".")), path)
        })
    });
    match rendered {
        Ok(output) => {
            print!("{output}");
            std::process::exit(0)
        }
        Err(why) => {
            eprintln!("loom-daemon forge {entity} view {}: {why}", query.number);
            std::process::exit(1)
        }
    }
}

fn decline() -> ! {
    crate::forge_cmd::decline_gitea(None);
    std::process::exit(DECLINED)
}

fn fields(entity: &str) -> Option<&'static [&'static str]> {
    match entity {
        "issue" => Some(ISSUE_FIELDS),
        "pr" => Some(PR_FIELDS),
        _ => None,
    }
}

/// The repository the read names: `--repo`, else `LOOM_REPO`, else the
/// checkout's origin.
fn repo(cwd: Option<&Path>, query: &ViewQuery) -> Result<String, String> {
    let env_repo = std::env::var("LOOM_REPO").ok().filter(|s| !s.is_empty());
    crate::forge_etag_store::resolve_target(cwd, query.repo.as_deref().or(env_repo.as_deref()))
        .repo
        .ok_or_else(|| {
            "the checkout's origin names no repository; pass --repo owner/repo".to_string()
        })
}

type Fetch<'a> = dyn FnMut(&str) -> Result<(u16, Value), String> + 'a;

fn render(
    entity: &str,
    query: &ViewQuery,
    repo: &str,
    fetch: &mut Fetch<'_>,
) -> Result<String, String> {
    let row = row(entity, query.number, repo, fetch)?;
    let object = project(entity, &row, query).ok_or_else(|| {
        format!("the answer is not a {entity} with the fields asked for: {}", keys(&row))
    })?;
    match &query.jq {
        Some(expr) => crate::forge_cached_list::apply_jq(&object, expr)
            .ok_or_else(|| format!("`jq` could not apply {expr}")),
        None => serde_json::to_string(&object)
            .map(|s| format!("{s}\n"))
            .map_err(|e| e.to_string()),
    }
}

/// The row of `number`, after settling its kind. A pull request is read from
/// `pulls/{n}`, which Gitea answers `404` for anything else; the issue
/// endpoint then says whether the number is an issue (named as the wrong
/// kind) or nothing this credential can see.
fn row(entity: &str, number: u32, repo: &str, fetch: &mut Fetch<'_>) -> Result<Value, String> {
    let issue = format!("repos/{repo}/issues/{number}");
    match entity {
        "pr" => match fetch(&format!("repos/{repo}/pulls/{number}"))? {
            (200, row) => Ok(row),
            (404, _) => match fetch(&issue)? {
                (200, found) if !is_pull(&found) => {
                    Err(format!("#{number} is an issue, not a pull request"))
                }
                (200, _) => Err(format!("#{number} is a pull request the API will not serve")),
                (404, _) => Err(format!("#{number} is not a pull request of {repo}")),
                (status, body) => Err(refused(status, &body)),
            },
            (status, body) => Err(refused(status, &body)),
        },
        _ => match fetch(&issue)? {
            (200, found) if is_pull(&found) => {
                Err(format!("#{number} is a pull request, not an issue"))
            }
            (200, found) => Ok(found),
            (404, _) => Err(format!("#{number} is not an issue of {repo}")),
            (status, body) => Err(refused(status, &body)),
        },
    }
}

fn is_pull(row: &Value) -> bool {
    row.get("pull_request").is_some_and(|p| !p.is_null())
}

fn refused(status: u16, body: &Value) -> String {
    match body["message"].as_str() {
        Some(message) if !message.is_empty() => {
            format!("the Gitea API answered {status}: {message}")
        }
        _ => format!("the Gitea API answered {status}"),
    }
}

fn keys(row: &Value) -> String {
    row.as_object()
        .map(|o| {
            let mut keys: Vec<&str> = o.keys().map(String::as_str).collect();
            keys.sort_unstable();
            keys.join(",")
        })
        .unwrap_or_else(|| "not an object".to_string())
}

fn id_of(value: &Value) -> Option<String> {
    value
        .as_u64()
        .map(|n| n.to_string())
        .or_else(|| value.as_str().map(str::to_string))
}

/// The row in `gh … view --json`'s shape, or `None` when the forge's answer
/// lacks a value gh would always have: the read then fails rather than
/// serving a blank a role could read as a fact.
fn project(entity: &str, row: &Value, query: &ViewQuery) -> Option<Value> {
    if row.get("number")?.as_u64()? != u64::from(query.number) {
        return None;
    }
    let text = |key: &str| Some(row.get(key)?.as_str()?.to_string());
    let at = |keys: &[&str]| {
        Some(
            keys.iter()
                .try_fold(row, |value, key| value.get(key))?
                .as_str()?
                .to_string(),
        )
    };
    // Gitea marshals an unset time as JSON `null`, older versions as the zero
    // time; gh prints `null` for both.
    let time = |key: &str| match row.get(key) {
        Some(Value::String(at)) if !at.starts_with("0001-01-01") => Some(at.clone()),
        _ => None,
    };
    let mut object = Map::new();
    for field in &query.json_fields {
        let value = match field.as_str() {
            "number" => json!(query.number),
            "title" | "body" => {
                json!(row.get(field).and_then(Value::as_str).unwrap_or(""))
            }
            "url" => json!(text("html_url")?),
            "state" => json!(state(entity, row)?),
            "labels" => labels(row)?,
            "createdAt" => json!(time("created_at")),
            "updatedAt" => json!(time("updated_at")),
            "closedAt" => json!(time("closed_at")),
            "mergedAt" => json!(time("merged_at")),
            "isDraft" => json!(row.get("draft").and_then(Value::as_bool).unwrap_or(false)),
            "headRefName" => json!(at(&["head", "ref"])?),
            "baseRefName" => json!(at(&["base", "ref"])?),
            "headRefOid" => json!(at(&["head", "sha"])?),
            "mergeable" => json!(mergeable(row)),
            _ => return None,
        };
        object.insert(field.clone(), value);
    }
    Some(Value::Object(object))
}

/// gh's `state`: `OPEN`/`CLOSED`, plus `MERGED` for a pull request Gitea
/// reports merged.
fn state(entity: &str, row: &Value) -> Option<&'static str> {
    if entity == "pr" && merged(row) {
        return Some("MERGED");
    }
    match row.get("state")?.as_str()? {
        "open" | "opened" => Some("OPEN"),
        "closed" => Some("CLOSED"),
        _ => None,
    }
}

fn merged(row: &Value) -> bool {
    row.get("merged") == Some(&Value::Bool(true))
        || matches!(
            row.get("merged_at").and_then(Value::as_str),
            Some(at) if !at.starts_with("0001-01-01")
        )
}

/// gh's `mergeable`: Gitea's bool, or `UNKNOWN` for anything else.
fn mergeable(row: &Value) -> &'static str {
    match row.get("mergeable") {
        Some(Value::Bool(true)) => "MERGEABLE",
        Some(Value::Bool(false)) => "CONFLICTING",
        _ => "UNKNOWN",
    }
}

/// Gitea's label objects as gh prints them: `{color, description, id, name}`.
fn labels(row: &Value) -> Option<Value> {
    Some(Value::Array(
        row.get("labels")?
            .as_array()?
            .iter()
            .map(|label| -> Option<Value> {
                Some(json!({
                    "color": label.get("color")?.as_str()?,
                    "description": label.get("description").and_then(Value::as_str).unwrap_or(""),
                    "id": id_of(label.get("id")?)?,
                    "name": label.get("name")?.as_str()?,
                }))
            })
            .collect::<Option<Vec<_>>>()?,
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const REPO: &str = "qual/consumer";
    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

    fn query(entity: &str, parts: &[&str]) -> ViewQuery {
        crate::forge_cached_view::parse_query_for(
            &parts.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
            fields(entity).unwrap(),
        )
        .unwrap()
    }

    fn pull(number: u64) -> Value {
        json!({
            "id": 9000 + number, "number": number, "state": "open", "title": "PR",
            "body": "A pull request", "html_url": "/qual/consumer/pulls/7",
            "labels": [{"id": 1, "name": "loom:review-requested", "color": "0075ca"}],
            "user": {"login": "alice"}, "draft": false, "mergeable": true,
            "head": {"ref": "feature/issue-1", "sha": HEAD},
            "base": {"ref": "main"},
            "created_at": "2026-11-20T10:00:00Z", "updated_at": "2026-11-21T10:00:00Z",
            "closed_at": null, "merged": false,
            "merged_at": "0001-01-01T00:00:00Z"
        })
    }

    fn issue(number: u64) -> Value {
        json!({
            "number": number, "state": "open", "labels": [],
            "created_at": "2026-11-20T10:00:00Z", "updated_at": "2026-11-21T10:00:00Z"
        })
    }

    /// A forge answering from `routes` (by request path), and the log of
    /// every path asked.
    struct Forge(Vec<(&'static str, u16, Value)>, Vec<String>);

    impl Forge {
        fn new(routes: Vec<(&'static str, u16, Value)>) -> Self {
            Self(routes, Vec::new())
        }

        fn fetch(&mut self, path: &str) -> Result<(u16, Value), String> {
            self.1.push(path.to_string());
            Ok(self
                .0
                .iter()
                .find(|(route, _, _)| path.ends_with(route))
                .map(|(_, status, value)| (*status, value.clone()))
                .unwrap_or((404, json!({"message": "Not Found"}))))
        }

        fn render(&mut self, entity: &str, parts: &[&str]) -> Result<String, String> {
            render(entity, &query(entity, parts), REPO, &mut |p| self.fetch(p))
        }
    }

    fn out(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn pr_view_is_ghs_shape_with_the_gate_fields() {
        let mut forge = Forge::new(vec![("/pulls/7", 200, pull(7))]);
        let text = forge
            .render(
                "pr",
                &[
                    "view",
                    "7",
                    "--json",
                    "number,state,labels,headRefOid,mergeable,headRefName,isDraft,createdAt,updatedAt,closedAt,mergedAt,title,body,url,baseRefName",
                ],
            )
            .unwrap();
        assert_eq!(
            text,
            format!(
                "{}\n",
                json!({
                    "baseRefName": "main", "body": "A pull request", "closedAt": null,
                    "createdAt": "2026-11-20T10:00:00Z", "headRefName": "feature/issue-1",
                    "headRefOid": HEAD, "isDraft": false,
                    "labels": [{"color": "0075ca", "description": "", "id": "1",
                                "name": "loom:review-requested"}],
                    "mergeable": "MERGEABLE", "mergedAt": null, "number": 7,
                    "state": "OPEN", "title": "PR", "updatedAt": "2026-11-21T10:00:00Z", "url": "/qual/consumer/pulls/7"
                })
            ),
            "compact JSON, the keys sorted as gh prints them, a trailing newline"
        );
        assert_eq!(forge.1, ["repos/qual/consumer/pulls/7"], "one read answers the gate");
    }

    #[test]
    fn state_follows_the_forge_and_unsets_are_null() {
        let mut closed = pull(8);
        closed["state"] = json!("closed");
        closed["closed_at"] = json!("2026-11-21T11:00:00Z");
        let mut merged = pull(9);
        merged["merged"] = json!(true);
        merged["merged_at"] = json!("2026-11-21T11:00:00Z");
        let mut fork = pull(10);
        fork.as_object_mut().unwrap().remove("updated_at");
        let mut forge = Forge::new(vec![
            ("/pulls/8", 200, closed),
            ("/pulls/9", 200, merged),
            ("/pulls/10", 200, fork),
            ("/issues/11", 200, issue(11)),
        ]);
        assert_eq!(
            out(&forge
                .render("pr", &["view", "8", "--json", "state,closedAt"])
                .unwrap())["state"],
            "CLOSED"
        );
        let merged = out(&forge
            .render("pr", &["view", "9", "--json", "state,mergedAt"])
            .unwrap());
        assert_eq!(merged["state"], "MERGED");
        assert_eq!(merged["mergedAt"], "2026-11-21T11:00:00Z");
        assert_eq!(
            out(&forge
                .render("pr", &["view", "10", "--json", "updatedAt,mergeable"])
                .unwrap())["updatedAt"],
            Value::Null,
            "an absent timestamp is null, never empty text"
        );
        assert_eq!(
            out(&forge
                .render("issue", &["view", "11", "--json", "state,labels"])
                .unwrap()),
            json!({"state": "OPEN", "labels": []})
        );
    }

    #[test]
    fn mergeability_is_giteas_bool_or_unknown() {
        let mut null = pull(9);
        null["mergeable"] = Value::Null;
        let mut conflicting = pull(8);
        conflicting["mergeable"] = json!(false);
        let mut absent = pull(10);
        absent.as_object_mut().unwrap().remove("mergeable");
        let mut forge = Forge::new(vec![
            ("/pulls/7", 200, pull(7)),
            ("/pulls/8", 200, conflicting),
            ("/pulls/9", 200, null),
            ("/pulls/10", 200, absent),
        ]);
        for (number, want) in [
            (7, "MERGEABLE"),
            (8, "CONFLICTING"),
            (9, "UNKNOWN"),
            (10, "UNKNOWN"),
        ] {
            let text = forge
                .render("pr", &["view", &number.to_string(), "--json", "mergeable"])
                .unwrap();
            assert_eq!(out(&text)["mergeable"], want, "#{number}");
        }
    }

    #[test]
    fn kind_is_settled_before_anything_is_served() {
        let mut forge = Forge::new(vec![
            ("/issues/9", 200, issue(9)),
            ("/issues/7", 200, json!({"number": 7, "pull_request": {"merged": false}})),
        ]);
        let err = forge
            .render("pr", &["view", "9", "--json", "state"])
            .unwrap_err();
        assert!(err.contains("#9 is an issue, not a pull request"), "{err}");
        assert_eq!(
            forge.1,
            [
                "repos/qual/consumer/pulls/9",
                "repos/qual/consumer/issues/9",
            ],
            "`pulls` is asked once, then the kind is asked of `issues`"
        );
        let err = forge
            .render("issue", &["view", "7", "--json", "state"])
            .unwrap_err();
        assert!(err.contains("#7 is a pull request, not an issue"), "{err}");
        assert_eq!(forge.1.last().unwrap(), "repos/qual/consumer/issues/7");

        let err = forge
            .render("pr", &["view", "55", "--json", "state"])
            .unwrap_err();
        assert!(err.contains("#55 is not a pull request of qual/consumer"), "{err}");
        let err = forge
            .render("issue", &["view", "55", "--json", "state"])
            .unwrap_err();
        assert!(err.contains("#55 is not an issue of qual/consumer"), "{err}");
    }

    #[test]
    fn a_refused_or_unreadable_answer_fails() {
        let mut forge = Forge::new(vec![
            ("/pulls/7", 401, json!({"message": "token is required"})),
            ("/pulls/8", 500, json!({})),
        ]);
        let err = forge
            .render("pr", &["view", "7", "--json", "state"])
            .unwrap_err();
        assert!(err.contains("401: token is required"), "{err}");
        let err = forge
            .render("pr", &["view", "8", "--json", "state"])
            .unwrap_err();
        assert_eq!(err, "the Gitea API answered 500");

        let mut headless = pull(9);
        headless.as_object_mut().unwrap().remove("head");
        let mut forge = Forge::new(vec![
            ("/pulls/9", 200, headless),
            ("/pulls/10", 200, json!({"number": 11, "state": "open"})),
        ]);
        let err = forge
            .render("pr", &["view", "9", "--json", "headRefOid"])
            .unwrap_err();
        assert!(err.contains("not a pr with the fields asked for"), "{err}");
        assert!(
            err.contains("number,state") && !err.contains("head,"),
            "{err} names the keys the answer holds, and `head` is not among them"
        );
        assert!(
            forge
                .render("pr", &["view", "10", "--json", "state"])
                .is_err(),
            "a number other than the one asked for"
        );
    }

    #[test]
    fn jq_is_applied_to_the_served_object() {
        let mut forge = Forge::new(vec![("/pulls/7", 200, pull(7))]);
        assert_eq!(
            forge
                .render("pr", &["view", "7", "--json", "labels", "--jq", ".labels[].name"])
                .unwrap(),
            "loom:review-requested\n"
        );
    }

    #[test]
    fn only_the_served_fields_parse() {
        for parts in [
            vec!["view", "7"],
            vec!["view", "7", "--json", "author"],
            vec!["view", "7", "--json", "id,state"],
            vec!["view", "7", "--json", "headRefOid,state", "--comments"],
        ] {
            assert!(
                fields("pr")
                    .and_then(|f| crate::forge_cached_view::parse_query_for(
                        &parts.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
                        f
                    ))
                    .is_none(),
                "{parts:?}"
            );
        }
        assert!(
            crate::forge_cached_view::parse_query_for(
                &["view", "7", "--cached", "--json", "state"]
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>(),
                PR_FIELDS
            )
            .is_some(),
            "`--cached` is accepted as a no-op: the read is live anyway"
        );
    }
}
