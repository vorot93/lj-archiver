fn assert_external_actions_use_full_shas(workflow: &str) {
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
    }
}

#[test]
fn publisher_revalidates_and_peels_the_remote_tag_before_the_sole_create() {
    let workflow = include_str!("../.github/workflows/release.yml");
    assert_eq!(workflow.matches("gh release create").count(), 1);
    let ref_query = workflow
        .find("git/ref/tags/$GITHUB_REF_NAME")
        .expect("publisher must query the live remote tag");
    let peel_query = workflow
        .find("git/tags/$tag_sha")
        .expect("publisher must peel annotated tag objects");
    let equality = workflow
        .find(r#"test "$tag_sha" = "$GITHUB_SHA""#)
        .expect("publisher must bind the peeled commit to the triggering SHA");
    let existence_check = workflow
        .find(r#"if gh release view "$GITHUB_REF_NAME""#)
        .expect("publisher must reject an existing release");
    let create = workflow.find("gh release create").unwrap();
    assert!(ref_query < peel_query);
    assert!(peel_query < equality);
    assert!(equality < existence_check);
    assert!(existence_check < create);
    assert!(workflow.contains("for peel_depth in 1 2 3 4 5 6 7 8"));

    assert_eq!(workflow.matches("\n  publish:\n").count(), 1);
    let publisher = workflow
        .split_once("\n  publish:\n")
        .expect("workflow must have one publisher")
        .1;
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
    assert!(workflow.contains("GH_REPO: ${{ github.repository }}"));

    assert!(workflow.contains(r#"test "${#all[@]}" -eq 5"#));
    assert!(workflow.contains(r#"test "${#deb[@]}" -eq 1"#));
    assert!(workflow.contains(r#"test "${#appimage[@]}" -eq 1"#));
    assert!(workflow.contains(r#"test "${#dmg[@]}" -eq 1"#));
    assert!(workflow.contains(r#"test "${#setup[@]}" -eq 1"#));
    assert!(workflow.contains(r#"test "${#msi[@]}" -eq 1"#));
    assert!(workflow.contains(r#"test "${#expected[@]}" -eq "${#all[@]}""#));

    assert_external_actions_use_full_shas(workflow);
}
