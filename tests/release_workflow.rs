/// Asserts every non-local `uses:` in `workflow` is pinned to a full 40-hex
/// commit SHA, and returns how many external Actions were checked so callers
/// can reject a vacuous pass (a wrong path or a parse that matched nothing).
fn assert_external_actions_use_full_shas(workflow: &str) -> usize {
    let mut checked = 0;
    for line in workflow.lines().filter(|line| line.contains("uses:")) {
        let target = line.split_once("uses:").unwrap().1.trim();
        if target.starts_with("./") {
            continue;
        }
        let target = target.split_whitespace().next().unwrap();
        let revision = target
            .rsplit_once('@')
            .map(|(_, revision)| revision)
            .expect("external Action must have a revision");
        assert_eq!(revision.len(), 40, "{target}");
        assert!(
            revision
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "{target}"
        );
        checked += 1;
    }
    checked
}

#[test]
fn ci_workflow_pins_every_external_action_to_a_full_sha() {
    // main.yml is invoked as the release `gates` job (workflow_call), so its
    // Actions are part of the release supply chain and must be SHA-pinned too
    // — release.yml's own pins are covered by the publisher test below.
    let checked = assert_external_actions_use_full_shas(include_str!("../.github/workflows/main.yml"));
    assert!(
        checked >= 1,
        "main.yml exposed no external Actions to verify; the pin check passed vacuously"
    );
}

#[test]
fn publisher_revalidates_and_peels_the_remote_tag_before_the_sole_create() {
    let workflow = include_str!("../.github/workflows/release.yml");
    assert_eq!(workflow.matches("gh release create").count(), 1);
    assert_eq!(workflow.matches("\n  publish:\n").count(), 1);
    let publisher = workflow
        .split_once("\n  publish:\n")
        .expect("workflow must have one publisher")
        .1;
    assert!(
        !publisher
            .lines()
            .any(|line| line.starts_with("  ") && !line.starts_with("    ")),
        "publish must remain the final job so its block is unambiguous"
    );

    assert_eq!(publisher.matches("gh release create").count(), 1);
    let ref_query = publisher
        .find("git/ref/tags/$GITHUB_REF_NAME")
        .expect("publisher must query the live remote tag");
    let peel_query = publisher
        .find("git/tags/$tag_sha")
        .expect("publisher must peel annotated tag objects");
    let equality = publisher
        .find(r#"test "$tag_sha" = "$GITHUB_SHA""#)
        .expect("publisher must bind the peeled commit to the triggering SHA");
    let master_query = publisher
        .find("git/ref/heads/master")
        .expect("publisher must query the live master ref");
    let comparison = publisher
        .find(r"compare/$GITHUB_SHA...$master_sha")
        .expect("publisher must compare exact commit identities, not a branch name");
    let existence_check = publisher
        .find(r#"if gh release view "$GITHUB_REF_NAME""#)
        .expect("publisher must reject an existing release");
    let create = publisher.find("gh release create").unwrap();
    assert!(ref_query < peel_query);
    assert!(peel_query < equality);
    assert!(equality < master_query);
    assert!(master_query < comparison);
    assert!(comparison < existence_check);
    assert!(existence_check < create);
    assert!(publisher.contains("for peel_depth in 1 2 3 4 5 6 7 8"));
    assert!(publisher.contains("GH_REPO: ${{ github.repository }}"));

    assert!(publisher.contains("set -euo pipefail"));
    assert!(publisher.contains("permissions:\n      contents: write"));
    assert!(!publisher.contains("actions/checkout"));
    assert_eq!(
        workflow
            .lines()
            .filter(|line| line.trim() == "contents: read")
            .count(),
        2
    );
    assert_eq!(
        workflow
            .lines()
            .filter(|line| line.trim() == "contents: write")
            .count(),
        1
    );
    assert!(workflow.contains("permissions:\n  contents: read"));
    assert!(workflow.contains("package:\n    needs: gates"));
    assert!(workflow.contains("publish:\n    needs: package"));

    assert!(workflow.contains(r#"test "${#all[@]}" -eq 5"#));
    assert!(workflow.contains(r#"test "${#deb[@]}" -eq 1"#));
    assert!(workflow.contains(r#"test "${#appimage[@]}" -eq 1"#));
    assert!(workflow.contains(r#"test "${#dmg[@]}" -eq 1"#));
    assert!(workflow.contains(r#"test "${#setup[@]}" -eq 1"#));
    assert!(workflow.contains(r#"test "${#msi[@]}" -eq 1"#));
    assert!(workflow.contains(r#"test "${#expected[@]}" -eq "${#all[@]}""#));

    assert!(
        assert_external_actions_use_full_shas(workflow) >= 1,
        "release.yml exposed no external Actions to verify; the pin check passed vacuously"
    );

    assert!(
        workflow
            .contains("git fetch --no-tags origin +refs/heads/master:refs/remotes/origin/master"),
        "master must be fetched through an explicit remote-tracking refspec"
    );
    assert!(
        !workflow.contains("git fetch --no-tags origin master\n"),
        "the bare fetch can leave a stale origin/master"
    );
    assert!(
        workflow
            .contains("git merge-base --is-ancestor \"$GITHUB_SHA\" refs/remotes/origin/master"),
        "the early check must use the freshly updated remote-tracking ref"
    );
}

/// Executes the shipped `Publish one immutable complete release` run block
/// against a fake `gh` on `PATH`, so these tests exercise the script's actual
/// logic rather than more string matching. Unix-only: the harness depends on
/// `bash` and on Unix file-permission bits (`PermissionsExt`) to mark the fake
/// `gh` executable; the Windows CI job runs a filtered `cargo test` subset
/// that skips this module, and a local Windows run must still compile.
#[cfg(unix)]
mod publisher_execution {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        process::{Command, Output},
    };

    /// Extracts the publisher step's `run:` block from the workflow and dedents it,
    /// so the tests below execute the *shipped* script rather than a copy that can
    /// drift away from it.
    fn publisher_run_script(workflow: &str) -> String {
        let step = workflow
            .split_once("      - name: Publish one immutable complete release\n")
            .expect("workflow must have the publisher step")
            .1;
        let body = step
            .split_once("        run: |\n")
            .expect("publisher step must have a run block")
            .1;

        body.lines()
            .take_while(|line| line.trim().is_empty() || line.starts_with("          "))
            .map(|line| line.strip_prefix("          ").unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n")
    }

    const TAG_REF: &str = "repos/o/r/git/ref/tags/v1.0.0";
    const MASTER_REF: &str = "repos/o/r/git/ref/heads/master";
    // Full 40-character lowercase-hex placeholders: the publisher's new
    // `grep -Eqx '[0-9a-f]{40}'` check on the master SHA rejects anything
    // shorter, so every "success" fixture needs a realistically-shaped SHA.
    const RELEASE_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const MASTER_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// Runs the publisher script with a fake `gh` on PATH. `responses` maps an
    /// exact `gh api <path>` argument to the stdout the fake returns; `fail` lists
    /// argument paths the fake exits nonzero for. `gh release view` exits 1 (no
    /// existing release) and `gh release create` records its invocation.
    fn run_publisher(script: &str, responses: &[(&str, &str)], fail: &[&str]) -> (Output, bool) {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(dir.path().join("dist")).unwrap();
        fs::write(dir.path().join("dist/a.deb"), b"x").unwrap();
        let created = dir.path().join("created");

        let mut cases = String::new();
        for path in fail {
            cases.push_str(&format!("    {path}) exit 1 ;;\n"));
        }
        for (path, body) in responses {
            cases.push_str(&format!("    {path}) printf '%s' '{body}' ;;\n"));
        }

        let gh = format!(
            r#"#!/usr/bin/env bash
set -eu
if [ "$1" = "api" ]; then
  case "$2" in
{cases}    *) echo "unexpected api $2" >&2; exit 1 ;;
  esac
  exit 0
fi
if [ "$1" = "release" ] && [ "$2" = "view" ]; then exit 1; fi
if [ "$1" = "release" ] && [ "$2" = "create" ]; then touch '{created}'; exit 0; fi
echo "unexpected gh $*" >&2
exit 1
"#,
            cases = cases,
            created = created.display(),
        );
        let gh_path = bin.join("gh");
        fs::write(&gh_path, gh).unwrap();
        fs::set_permissions(&gh_path, fs::Permissions::from_mode(0o755)).unwrap();

        let script_path = dir.path().join("publish.sh");
        fs::write(&script_path, script).unwrap();

        let output = Command::new("bash")
            .arg(&script_path)
            .current_dir(dir.path())
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .env("GH_REPO", "o/r")
            .env("GH_TOKEN", "t")
            .env("GITHUB_REF_NAME", "v1.0.0")
            .env("GITHUB_SHA", RELEASE_SHA)
            .output()
            .unwrap();
        let created = created.exists();

        (output, created)
    }

    fn tag_response(sha: &str) -> String {
        format!("commit\t{sha}")
    }

    /// One row of a rejection-scenario table: a case name, the fake `gh api`
    /// responses to serve, and the paths that should fail outright.
    type RejectionCase<'a> = (&'a str, Vec<(&'a str, String)>, Vec<&'a str>);

    fn compare_response(
        status: &str,
        base: &str,
        merge_base: &str,
        count: usize,
        head: &str,
    ) -> String {
        format!("{status}\t{base}\t{merge_base}\t{count}\t{head}")
    }

    #[test]
    fn publisher_creates_the_release_when_the_commit_is_still_on_master() {
        let script = publisher_run_script(include_str!("../.github/workflows/release.yml"));

        for (status, count, head) in [("ahead", 3, MASTER_SHA), ("identical", 0, "")] {
            let master = if status == "identical" {
                RELEASE_SHA
            } else {
                MASTER_SHA
            };
            let compare_path = format!("repos/o/r/compare/{RELEASE_SHA}...{master}");
            let (output, created) = run_publisher(
                &script,
                &[
                    (TAG_REF, &tag_response(RELEASE_SHA)),
                    (MASTER_REF, &tag_response(master)),
                    (
                        &compare_path,
                        &compare_response(status, RELEASE_SHA, RELEASE_SHA, count, head),
                    ),
                ],
                &[],
            );

            assert!(
                output.status.success(),
                "{status}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(created, "{status}: release was not created");
        }
    }

    #[test]
    fn publisher_refuses_to_create_a_release_when_master_ancestry_is_gone() {
        let script = publisher_run_script(include_str!("../.github/workflows/release.yml"));
        let compare_path = format!("repos/o/r/compare/{RELEASE_SHA}...{MASTER_SHA}");
        let malformed_compare_path = format!("repos/o/r/compare/{RELEASE_SHA}...not-a-sha");

        let cases: Vec<RejectionCase> = vec![
            // Removed from master: the commit is no longer an ancestor.
            (
                "diverged",
                vec![
                    (TAG_REF, tag_response(RELEASE_SHA)),
                    (MASTER_REF, tag_response(MASTER_SHA)),
                    (
                        compare_path.as_str(),
                        compare_response("diverged", RELEASE_SHA, "cccc", 2, MASTER_SHA),
                    ),
                ],
                vec![],
            ),
            // Master is behind the release commit.
            (
                "behind",
                vec![
                    (TAG_REF, tag_response(RELEASE_SHA)),
                    (MASTER_REF, tag_response(MASTER_SHA)),
                    (
                        compare_path.as_str(),
                        compare_response("behind", RELEASE_SHA, MASTER_SHA, 0, ""),
                    ),
                ],
                vec![],
            ),
            // Merge base is not the release commit.
            (
                "merge base mismatch",
                vec![
                    (TAG_REF, tag_response(RELEASE_SHA)),
                    (MASTER_REF, tag_response(MASTER_SHA)),
                    (
                        compare_path.as_str(),
                        compare_response("ahead", RELEASE_SHA, "cccc", 1, MASTER_SHA),
                    ),
                ],
                vec![],
            ),
            // Head commit is not the queried master SHA.
            (
                "head mismatch",
                vec![
                    (TAG_REF, tag_response(RELEASE_SHA)),
                    (MASTER_REF, tag_response(MASTER_SHA)),
                    (
                        compare_path.as_str(),
                        compare_response("ahead", RELEASE_SHA, RELEASE_SHA, 1, "dddd"),
                    ),
                ],
                vec![],
            ),
            // Malformed master SHA. A response is registered for the compare
            // call this would reach if the 40-hex format check were missing,
            // so the case fails *because that check fired*, not because the
            // fake `gh` happens to reject an unregistered path.
            (
                "malformed master sha",
                vec![
                    (TAG_REF, tag_response(RELEASE_SHA)),
                    (MASTER_REF, "commit\tnot-a-sha".to_string()),
                    (
                        malformed_compare_path.as_str(),
                        compare_response("ahead", RELEASE_SHA, RELEASE_SHA, 1, "not-a-sha"),
                    ),
                ],
                vec![],
            ),
            // Master ref is not a commit. Same trap: register the compare
            // response the script would reach if the `master_type = commit`
            // check were missing, so a missing check is what makes this fail.
            (
                "master ref is not a commit",
                vec![
                    (TAG_REF, tag_response(RELEASE_SHA)),
                    (MASTER_REF, format!("tag\t{MASTER_SHA}")),
                    (
                        compare_path.as_str(),
                        compare_response("ahead", RELEASE_SHA, RELEASE_SHA, 1, MASTER_SHA),
                    ),
                ],
                vec![],
            ),
            // The tag no longer points at the release commit. Same trap as the
            // cases above: register the master and compare responses the script
            // would reach if the tag-equality guard were missing, so this case
            // fails *because that guard fired* — with a consistent, otherwise
            // release-worthy master ahead of the tag — not because the fake `gh`
            // rejects an unregistered path.
            (
                "tag mismatch",
                vec![
                    (TAG_REF, tag_response("eeee")),
                    (MASTER_REF, tag_response(MASTER_SHA)),
                    (
                        compare_path.as_str(),
                        compare_response("ahead", RELEASE_SHA, RELEASE_SHA, 1, MASTER_SHA),
                    ),
                ],
                vec![],
            ),
        ];

        for (name, responses, fail) in cases {
            let responses = responses
                .iter()
                .map(|(path, body)| (*path, body.as_str()))
                .collect::<Vec<_>>();
            let (output, created) = run_publisher(&script, &responses, &fail);

            assert!(!output.status.success(), "{name}: publisher succeeded");
            assert!(!created, "{name}: a release was created anyway");
        }
    }

    #[test]
    fn publisher_refuses_to_create_a_release_when_the_api_fails() {
        let script = publisher_run_script(include_str!("../.github/workflows/release.yml"));
        let compare_path = format!("repos/o/r/compare/{RELEASE_SHA}...{MASTER_SHA}");

        for failing in [TAG_REF, MASTER_REF, compare_path.as_str()] {
            let (output, created) = run_publisher(
                &script,
                &[
                    (TAG_REF, &tag_response(RELEASE_SHA)),
                    (MASTER_REF, &tag_response(MASTER_SHA)),
                    (
                        compare_path.as_str(),
                        &compare_response("ahead", RELEASE_SHA, RELEASE_SHA, 1, MASTER_SHA),
                    ),
                ],
                &[failing],
            );

            assert!(!output.status.success(), "{failing}: publisher succeeded");
            assert!(!created, "{failing}: a release was created anyway");
        }
    }

    /// The fake `gh` above hands back pre-rendered TSV directly and never runs
    /// real `jq`, so it cannot catch a typo in the shipped `--jq` filter. This
    /// extracts that exact filter string from the shipped script and runs it
    /// through real `jq` against a response shaped like the documented Compare
    /// Commits API schema (see GitHub REST API docs, "Compare two commits"):
    /// `status`/`base_commit`/`merge_base_commit`/`commits` at the top level,
    /// no `head_commit` field, and `commits` in chronological order.
    #[test]
    fn compare_jq_filter_extracts_status_and_shas_from_a_realistic_response() {
        let script = publisher_run_script(include_str!("../.github/workflows/release.yml"));
        let line = script
            .lines()
            .find(|line| line.contains("compare/$GITHUB_SHA...$master_sha"))
            .expect("script must call the compare API");
        let filter = line
            .split('\'')
            .nth(1)
            .expect("the compare call must have a single-quoted --jq filter");

        let run_jq = |json: &str| {
            let output = Command::new("jq")
                .arg(filter)
                .arg("-r")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write;
                    child.stdin.take().unwrap().write_all(json.as_bytes())?;
                    child.wait_with_output()
                })
                .expect("jq must run");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8(output.stdout).unwrap();
            // Strip only the newline jq appends: an empty final TSV field is
            // significant (it is exactly the "identical" case below), and a
            // blanket trim_end() would silently eat that trailing tab too.
            stdout.strip_suffix('\n').unwrap_or(&stdout).to_string()
        };

        let ahead = r#"{
            "status": "ahead",
            "ahead_by": 3,
            "behind_by": 0,
            "total_commits": 3,
            "base_commit": {"sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
            "merge_base_commit": {"sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
            "commits": [
                {"sha": "c111111111111111111111111111111111111111"},
                {"sha": "c222222222222222222222222222222222222222"},
                {"sha": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}
            ]
        }"#;
        assert_eq!(
            run_jq(ahead),
            "ahead\taaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\taaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\t3\tbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        );

        let identical = r#"{
            "status": "identical",
            "ahead_by": 0,
            "behind_by": 0,
            "total_commits": 0,
            "base_commit": {"sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
            "merge_base_commit": {"sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
            "commits": []
        }"#;
        assert_eq!(
            run_jq(identical),
            "identical\taaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\taaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\t0\t"
        );
    }
}
