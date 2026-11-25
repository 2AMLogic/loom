//! Structural checks of GitHub Actions workflow files: the
//! `permissions: write-all` setting (#14648) and evaluated secret use
//! (#14915). The Champion caller owns head pinning and its other capability
//! checks.

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read};

const MAX_WORKFLOW_BYTES: usize = 1024 * 1024;

#[derive(Deserialize)]
struct Workflow {
    #[serde(default)]
    permissions: Permissions,
    jobs: BTreeMap<String, Job>,
}

#[derive(Deserialize)]
struct Job {
    #[serde(default)]
    permissions: Permissions,
}

#[derive(Default, Deserialize)]
#[serde(untagged)]
enum Permissions {
    #[default]
    #[serde(skip)]
    Unspecified,
    All(String),
    Scopes(BTreeMap<String, String>),
}

impl Permissions {
    fn is_write_all(&self) -> anyhow::Result<bool> {
        match self {
            Self::Unspecified => Ok(false),
            Self::All(value) if value.trim().eq_ignore_ascii_case("write-all") => Ok(true),
            Self::All(value) if value.trim().eq_ignore_ascii_case("read-all") => Ok(false),
            // Named write scopes still go through Champion's existing policy.
            Self::Scopes(scopes)
                if scopes
                    .values()
                    .all(|value| matches!(value.as_str(), "read" | "write" | "none")) =>
            {
                Ok(false)
            }
            _ => anyhow::bail!("unrecognized permissions value"),
        }
    }
}

fn contains_write_all(input: &[u8]) -> anyhow::Result<bool> {
    anyhow::ensure!(input.len() <= MAX_WORKFLOW_BYTES, "workflow exceeds the input size limit");
    let text = std::str::from_utf8(input)?;
    let options = serde_saphyr::options! {
        with_snippet: false,
        reject_unsupported_tags: true,
        budget: serde_saphyr::budget! {
            max_reader_input_bytes: Some(MAX_WORKFLOW_BYTES),
            max_documents: 1,
        },
    };
    let workflow: Workflow = serde_saphyr::from_str_with_options(text, options)
        .map_err(|_| anyhow::anyhow!("unreadable workflow YAML"))?;
    let mut has_write_all = workflow.permissions.is_write_all()?;
    for job in workflow.jobs.values() {
        has_write_all |= job.permissions.is_write_all()?;
    }
    Ok(has_write_all)
}

/// Which structural check `run` applies to the workflow on stdin.
pub(super) enum Check {
    WriteAll,
    SecretUse,
}

/// Any YAML node, walked generically so literal text stays distinguishable
/// from evaluated content (#14915). Non-string scalars carry no expression.
enum Node {
    Str(String),
    Other,
    Seq(Vec<Node>),
    Map(Vec<(Node, Node)>),
}

impl<'de> Deserialize<'de> for Node {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct AnyNode;
        impl<'de> Visitor<'de> for AnyNode {
            type Value = Node;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any YAML node")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(Node::Str(v.to_owned()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(Node::Str(v))
            }
            fn visit_bool<E: de::Error>(self, _v: bool) -> Result<Self::Value, E> {
                Ok(Node::Other)
            }
            fn visit_i64<E: de::Error>(self, _v: i64) -> Result<Self::Value, E> {
                Ok(Node::Other)
            }
            fn visit_u64<E: de::Error>(self, _v: u64) -> Result<Self::Value, E> {
                Ok(Node::Other)
            }
            fn visit_f64<E: de::Error>(self, _v: f64) -> Result<Self::Value, E> {
                Ok(Node::Other)
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(Node::Other)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element::<Node>()? {
                    items.push(item);
                }
                Ok(Node::Seq(items))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut pairs = Vec::new();
                while let Some((key, value)) = map.next_entry::<Node, Node>()? {
                    pairs.push((key, value));
                }
                Ok(Node::Map(pairs))
            }
        }
        deserializer.deserialize_any(AnyNode)
    }
}

fn contains_secret_use(input: &[u8]) -> anyhow::Result<bool> {
    anyhow::ensure!(input.len() <= MAX_WORKFLOW_BYTES, "workflow exceeds the input size limit");
    let text = std::str::from_utf8(input)?;
    let options = serde_saphyr::options! {
        with_snippet: false,
        reject_unsupported_tags: true,
        budget: serde_saphyr::budget! {
            max_reader_input_bytes: Some(MAX_WORKFLOW_BYTES),
            max_documents: 1,
        },
    };
    let root: Option<Node> = serde_saphyr::from_str_with_options(text, options)
        .map_err(|_| anyhow::anyhow!("unreadable workflow YAML"))?;
    // An empty (or null) document is not a workflow: unreadable, not clear.
    let root = root.ok_or_else(|| anyhow::anyhow!("empty workflow YAML"))?;
    Ok(node_has_secret_use(&root))
}

fn node_has_secret_use(node: &Node) -> bool {
    match node {
        Node::Str(text) => expression_references_secret(text),
        Node::Other => false,
        Node::Seq(items) => items.iter().any(node_has_secret_use),
        Node::Map(pairs) => pairs.iter().any(|(key, value)| {
            // `secrets: inherit` — the inherited-secrets hold (#12988),
            // classified structurally: only the YAML key inherits, not scalar
            // text that merely spells the words.
            (matches!(key, Node::Str(k) if k.eq_ignore_ascii_case("secrets"))
                && matches!(value, Node::Str(v) if v.trim().eq_ignore_ascii_case("inherit")))
                || node_has_secret_use(key)
                || node_has_secret_use(value)
        }),
    }
}

/// A `secrets` context inside a `${{ … }}` expression of evaluated scalar
/// text. Expressions embedded in longer text are scanned too; text with no
/// expression is literal and clears (#14915). An unterminated `${{` tail is
/// treated as an expression (fail closed).
fn expression_references_secret(text: &str) -> bool {
    let mut rest = text;
    while let Some(start) = rest.find("${{") {
        let body = &rest[start + 3..];
        match body.find("}}") {
            Some(end) => {
                if expression_body_references_secret(&body[..end]) {
                    return true;
                }
                rest = &body[end + 2..];
            }
            None => return expression_body_references_secret(body),
        }
    }
    false
}

/// The `secrets` context inside one expression body: any use holds (a named
/// secret, a bracket lookup, or the bare context such as `toJSON(secrets)`)
/// except the built-in `secrets.GITHUB_TOKEN` access, which keeps the exact
/// case-and-boundary exception of the regex it replaces (#12995).
fn expression_body_references_secret(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut from = 0;
    while let Some(offset) = lower[from..].find("secrets") {
        let start = from + offset;
        let after = start + "secrets".len();
        from = after;
        let boundary_before = start == 0 || !is_word_byte(bytes[start - 1]);
        if !boundary_before {
            continue;
        }
        let tail = &body[after..];
        if tail.starts_with(is_word_char) {
            continue; // a longer identifier, not the context
        }
        if let Some(rest) = tail.strip_prefix('.') {
            let (name, tail) = split_identifier(rest);
            if name == "GITHUB_TOKEN" && !tail.starts_with(is_word_char) {
                continue; // the built-in token (#12995)
            }
            return true;
        }
        if let Some(rest) = tail.strip_prefix('[') {
            if bracket_access_is_builtin(rest.trim_start()) {
                continue; // the built-in token, bracket form (#12995)
            }
            return true;
        }
        return true;
    }
    false
}

/// `secrets["GITHUB_TOKEN"]` and `secrets['GITHUB_TOKEN']` (or an unquoted
/// key): the bracket form of the built-in exception.
fn bracket_access_is_builtin(rest: &str) -> bool {
    let inner = match rest.chars().next() {
        Some(quote @ ('"' | '\'')) => {
            let Some(end) = rest[quote.len_utf8()..].find(quote) else {
                return false;
            };
            &rest[quote.len_utf8()..quote.len_utf8() + end]
        }
        _ => rest,
    };
    let (name, _) = split_identifier(inner);
    name == "GITHUB_TOKEN"
}

fn split_identifier(text: &str) -> (&str, &str) {
    let end = text
        .char_indices()
        .find(|(_, c): &(usize, char)| !is_word_char(*c))
        .map_or(text.len(), |(i, _)| i);
    text.split_at(end)
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

pub(super) fn run(check: Check) -> ! {
    let mut input = Vec::new();
    let result = match io::stdin()
        .lock()
        .take(MAX_WORKFLOW_BYTES as u64 + 1)
        .read_to_end(&mut input)
    {
        Ok(_) => match check {
            Check::WriteAll => contains_write_all(&input),
            Check::SecretUse => contains_secret_use(&input),
        },
        Err(_) => Err(anyhow::anyhow!("unreadable input")),
    };
    let code = match result {
        Ok(false) => 0,
        Ok(true) => 1,
        Err(_) => 2,
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAME_ONLY: &[u8] = include_bytes!(
        "../../../defaults/scripts/tests/fixtures/champion-workflows/ci-write-all-test-name.yml"
    );
    const JOB_PERMISSION: &[u8] = include_bytes!(
        "../../../defaults/scripts/tests/fixtures/champion-workflows/ci-write-all-permission.yml"
    );

    #[test]
    fn distinguishes_test_text_from_permissions() {
        assert!(matches!(contains_write_all(NAME_ONLY), Ok(false)));
        assert!(matches!(contains_write_all(JOB_PERMISSION), Ok(true)));
        for text in [
            "permissions: write-all\njobs: {check: {}}\n",
            "'permissions': \"write-all\"\njobs: {check: {}}\n",
            "permissions:\n  write-all\njobs: {check: {}}\n",
            "permissions: >\n  write-all\njobs: {check: {}}\n",
            "jobs: {check: {permissions: write-all}}\n",
            "env: {ALL: &all write-all}\njobs: {check: {permissions: *all}}\n",
        ] {
            assert!(matches!(contains_write_all(text.as_bytes()), Ok(true)), "{text}");
        }
    }

    #[test]
    fn leaves_read_only_and_named_scopes_to_existing_policy() {
        for text in [
            "permissions: read-all\njobs: {check: {}}\n",
            "permissions: {}\njobs: {check: {}}\n",
            "permissions: {contents: write}\njobs: {check: {}}\n",
            "jobs: {check: {env: {permissions: write-all}}}\n",
        ] {
            assert!(matches!(contains_write_all(text.as_bytes()), Ok(false)), "{text}");
        }
    }

    #[test]
    fn rejects_unreadable_workflows() {
        for input in [
            b"".as_slice(),
            b"\xff",
            b"jobs: [",
            b"name: not a workflow\n",
            b"permissions: null\njobs: {check: {}}\n",
            b"permissions: unknown\njobs: {check: {}}\n",
            b"permissions: read-all\npermissions: write-all\njobs: {check: {}}\n",
            b"jobs: {check: {}}\n---\njobs: {check: {permissions: write-all}}\n",
        ] {
            assert!(contains_write_all(input).is_err(), "{input:?}");
        }
        assert!(contains_write_all(&vec![b' '; MAX_WORKFLOW_BYTES + 1]).is_err());
    }

    const PATH_FILTER_NAME: &[u8] = include_bytes!(
        "../../../defaults/scripts/tests/fixtures/champion-workflows/ci-pathfilter-deploy-secrets.yml"
    );
    const STEP_NAME: &[u8] = include_bytes!(
        "../../../defaults/scripts/tests/fixtures/champion-workflows/ci-stepname-secrets-tests.yml"
    );
    const CLOUDFLARE_SECRET: &[u8] = include_bytes!(
        "../../../defaults/scripts/tests/fixtures/champion-workflows/ci-with-cloudflare-secret.yml"
    );
    const GITHUB_TOKEN_ONLY: &[u8] = include_bytes!(
        "../../../defaults/scripts/tests/fixtures/champion-workflows/ci-github-token-only.yml"
    );

    #[test]
    fn literal_workflow_text_is_not_secret_use() {
        // 2AMLogic/2am#7995: the two reported literals, independently.
        assert!(matches!(contains_secret_use(PATH_FILTER_NAME), Ok(false)));
        assert!(matches!(contains_secret_use(STEP_NAME), Ok(false)));
        assert!(matches!(contains_secret_use(GITHUB_TOKEN_ONLY), Ok(false)));
        for text in [
            // A YAML 1.1 bool key (`on`) must not make the walk unreadable.
            "on: {pull_request: {paths: ['scripts/deploy_secrets.py']}}\n",
            "# secrets.sh tests\njobs: {check: {steps: [{name: secrets.sh tests}]}}\n",
            "jobs: {check: {steps: [{name: secrets.sh tests}]}}\n",
            "jobs:\n  check:\n    steps:\n      - run: make test  # deploy_secrets.py first\n",
            // Literal text that spells the inherited-secrets shape.
            "run: 'echo secrets: inherit'\n",
            "run: see docs/secrets.md and ${{ matrix.os }}\n",
            "x: ${{ github.token }}\n",
            "x: ${{ env.SECRETS_FILE }}\n",
            "x: ${{ inputs.mysecrets_value }}\n",
            "x: ${{ secrets.GITHUB_TOKEN }}\n",
            "x: ${{secrets.GITHUB_TOKEN}}\n",
            "x: ${{ secrets['GITHUB_TOKEN'] }}\n",
            "x: ${{ secrets[\"GITHUB_TOKEN\"] }}\n",
            "x: ${{ format('{0}', secrets.GITHUB_TOKEN) }}\n",
            "x: ${{ secrets.GITHUB_TOKEN.SUFFIX }}\n",
        ] {
            assert!(matches!(contains_secret_use(text.as_bytes()), Ok(false)), "{text}");
        }
    }

    #[test]
    fn evaluated_secret_references_hold() {
        assert!(matches!(contains_secret_use(CLOUDFLARE_SECRET), Ok(true)));
        for text in [
            // An expression embedded in scalar text, a multiline block scalar.
            "run: echo ${{ secrets.PAT }}\n",
            "run: |\n  echo ${{ secrets.PAT }} and\n  ${{ toJSON(secrets) }}\n",
            "x: ${{ toJSON(secrets) }}\n",
            "x: ${{ secrets[\"PAT\"] }}\n",
            "x: ${{ secrets['PAT'] }}\n",
            // A built-in token access must not clear the rest of its expression.
            "x: ${{ secrets['GITHUB_TOKEN'] || secrets.DEPLOY_KEY }}\n",
            "x: ${{ format('{0}{1}', secrets[\"GITHUB_TOKEN\"], secrets.PAT) }}\n",
            // Contexts are case-insensitive in expressions.
            "x: ${{ SECRETS.PAT }}\n",
            // The built-in exception keeps its exact case and boundary (#12995).
            "x: ${{ secrets.github_token }}\n",
            "x: ${{ secrets.GITHUB_TOKEN2 }}\n",
            // The inherited-secrets hold, classified structurally.
            "jobs: {check: {secrets: inherit}}\n",
            "jobs: {check: {secrets: 'inherit'}}\n",
            // An unterminated expression is indistinguishable from a real one.
            "x: ${{ secrets.PAT\n",
        ] {
            assert!(matches!(contains_secret_use(text.as_bytes()), Ok(true)), "{text}");
        }
    }

    #[test]
    fn rejects_unreadable_workflows_for_secret_use() {
        for input in [
            b"".as_slice(),
            b"\xff",
            b"jobs: [",
            b"x: 1\nx: 2\n",
            b"x: 1\n---\nx: 2\n",
            b"x: !custom 1\n",
        ] {
            assert!(contains_secret_use(input).is_err(), "{input:?}");
        }
        assert!(contains_secret_use(&vec![b' '; MAX_WORKFLOW_BYTES + 1]).is_err());
    }
}
