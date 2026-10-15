use super::*;

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

#[tokio::test]
async fn history_sync_stash_tags_and_conflicts() {
    let f = Fixture::new("127.0.0.1:8787".parse().unwrap(), None);
    let id = register(&f).await;
    let base = format!("/api/workspaces/{id}/git");
    let repo = f.path.join("repo");
    let at = |path: &str| format!("{base}/{path}");

    // No commits yet: an empty history, not an error.
    let (status, log) = call(f.app(), "GET", &at("log"), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{log}");
    assert_eq!(log["commits"], json!([]));

    fs::write(repo.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-qm", "first"]);
    let home = git(&repo, &["symbolic-ref", "--short", "HEAD"]);
    fs::write(repo.join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
    fs::write(repo.join("b.txt"), "bee\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "second\n\nWith a body."]);

    let (_, log) = call(f.app(), "GET", &at("log?limit=1"), Value::Null).await;
    assert_eq!(log["commits"][0]["subject"], "second");
    assert_eq!(log["more"], true);
    assert!(
        log["commits"][0]["refs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r.as_str().unwrap().contains(&home))
    );
    let head = log["commits"][0]["hash"].as_str().unwrap().to_owned();

    let (status, detail) = call(
        f.app(),
        "GET",
        &at(&format!("commit/detail?rev={head}")),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["message"], "second\n\nWith a body.");
    let files = detail["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    let a = files.iter().find(|f| f["path"] == "a.txt").unwrap();
    assert_eq!(
        (
            a["status"].as_str(),
            a["added"].as_u64(),
            a["removed"].as_u64()
        ),
        (Some("M"), Some(1), Some(0))
    );
    let (_, diff) = call(
        f.app(),
        "GET",
        &at(&format!("commit/diff?rev={head}&path=a.txt")),
        Value::Null,
    )
    .await;
    assert!(diff["diff"].as_str().unwrap().contains("+four"));
    // A root commit has no parent to diff against, and still shows its files.
    let (_, root) = call(f.app(), "GET", &at("commit/detail?rev=HEAD~1"), Value::Null).await;
    assert_eq!(root["files"][0]["status"], "A");
    for bad in ["--output=x", "HEAD;rm", "nope"] {
        let (status, _) = call(
            f.app(),
            "GET",
            &at(&format!("commit/detail?rev={bad}")),
            Value::Null,
        )
        .await;
        assert!(status.is_client_error(), "{bad}");
    }

    // Amend replaces the last commit rather than adding one.
    let (_, amended) = call(
        f.app(),
        "POST",
        &at("commit"),
        json!({"message":"second, amended","amend":true}),
    )
    .await;
    assert_ne!(amended["commit"], head);
    assert_eq!(git(&repo, &["rev-list", "--count", "HEAD"]), "2");

    // One hunk staged out of a file, then taken back out.
    fs::write(repo.join("a.txt"), "zero\none\ntwo\nthree\nfour\n").unwrap();
    let (_, unstaged) = call(f.app(), "GET", &at("diff?path=a.txt"), Value::Null).await;
    let patch = unstaged["diff"].as_str().unwrap().to_owned();
    let (status, body) = call(
        f.app(),
        "POST",
        &at("apply"),
        json!({"path":"a.txt","patch":patch,"cached":true}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(git(&repo, &["diff", "--cached"]).contains("+zero"));
    let (status, _) = call(
        f.app(),
        "POST",
        &at("apply"),
        json!({"path":"a.txt","patch":patch,"cached":true,"reverse":true}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(git(&repo, &["diff", "--cached"]).is_empty());
    let (status, _) = call(
        f.app(),
        "POST",
        &at("apply"),
        json!({"path":".git/config","patch":patch,"cached":true}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Stash with an untracked file, list it, read it, pop it back.
    fs::write(repo.join("new.txt"), "fresh\n").unwrap();
    let (status, stashes) = call(
        f.app(),
        "POST",
        &at("stashes"),
        json!({"message":"wip","include_untracked":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{stashes}");
    assert!(stashes[0]["message"].as_str().unwrap().contains("wip"));
    assert_eq!(stashes[0]["untracked"], true);
    assert!(!repo.join("new.txt").exists());
    let stash = stashes[0]["hash"].as_str().unwrap().to_owned();
    let (_, shown) = call(
        f.app(),
        "GET",
        &at(&format!("commit/detail?rev={stash}&stash=true")),
        Value::Null,
    )
    .await;
    let names: Vec<_> = shown["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"a.txt") && names.contains(&"new.txt"),
        "{names:?}"
    );
    let (_, untracked) = call(
        f.app(),
        "GET",
        &at(&format!(
            "commit/diff?rev={stash}&path=new.txt&untracked=true"
        )),
        Value::Null,
    )
    .await;
    assert!(untracked["diff"].as_str().unwrap().contains("+fresh"));
    let (status, _) = call(
        f.app(),
        "POST",
        &at("stashes/apply"),
        json!({"index":0,"hash":"0000","pop":true}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "a stale stash is refused");
    let (status, popped) = call(
        f.app(),
        "POST",
        &at("stashes/apply"),
        json!({"index":0,"hash":stash,"pop":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{popped}");
    assert!(repo.join("new.txt").exists());
    let (_, stashes) = call(f.app(), "GET", &at("stashes"), Value::Null).await;
    assert_eq!(stashes, json!([]));
    let (status, _) = call(f.app(), "POST", &at("ignore"), json!({"path":"new.txt"})).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        fs::read_to_string(repo.join(".gitignore"))
            .unwrap()
            .contains("/new.txt")
    );
    git(&repo, &["checkout", "--", "a.txt"]);
    fs::remove_file(repo.join(".gitignore")).unwrap();

    // Tags, plain and annotated.
    let (status, tags) = call(
        f.app(),
        "POST",
        &at("tags"),
        json!({"name":"v1","rev":"HEAD~1","message":"First"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{tags}");
    assert_eq!(
        (tags[0]["name"].as_str(), tags[0]["annotated"].as_bool()),
        (Some("v1"), Some(true))
    );
    assert_eq!(
        tags[0]["hash"].as_str().unwrap(),
        git(&repo, &["rev-parse", "HEAD~1"])
    );
    assert_eq!(
        call(f.app(), "POST", &at("tags"), json!({"name":"-bad"}))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let (_, tags) = call(f.app(), "POST", &at("tags/delete"), json!({"name":"v1"})).await;
    assert_eq!(tags, json!([]));

    // A conflicting merge stops with the conflict in the working tree,
    // resolves by taking one side, and continues.
    git(&repo, &["switch", "-qc", "side"]);
    fs::write(repo.join("b.txt"), "side\n").unwrap();
    git(&repo, &["commit", "-qam", "side change"]);
    git(&repo, &["switch", "-q", &home]);
    fs::write(repo.join("b.txt"), "home\n").unwrap();
    git(&repo, &["commit", "-qam", "home change"]);
    let (status, merged) = call(f.app(), "POST", &at("merge"), json!({"rev":"side"})).await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    assert_eq!(merged["conflicts"], true);
    let (_, current) = call(f.app(), "GET", &at("status"), Value::Null).await;
    assert_eq!(current["operation"], "merge");
    assert_eq!(current["files"][0]["index"], "U");
    let (status, _) = call(
        f.app(),
        "POST",
        &at("operation"),
        json!({"action":"continue"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "unresolved conflicts hold the merge"
    );
    let (status, _) = call(
        f.app(),
        "POST",
        &at("resolve"),
        json!({"path":"b.txt","side":"theirs"}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(fs::read_to_string(repo.join("b.txt")).unwrap(), "side\n");
    let (status, done) = call(
        f.app(),
        "POST",
        &at("operation"),
        json!({"action":"continue"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    let (_, current) = call(f.app(), "GET", &at("status"), Value::Null).await;
    assert!(current.get("operation").is_none());
    assert_eq!(
        git(&repo, &["rev-list", "--parents", "-n1", "HEAD"])
            .split(' ')
            .count(),
        3
    );

    // Revert, cherry-pick and reset from history.
    let picked = git(&repo, &["rev-parse", "side"]);
    let (status, reverted) = call(f.app(), "POST", &at("revert"), json!({"rev":"HEAD"})).await;
    assert_eq!(status, StatusCode::OK, "{reverted}");
    assert_eq!(fs::read_to_string(repo.join("b.txt")).unwrap(), "home\n");
    let (status, _) = call(
        f.app(),
        "POST",
        &at("reset"),
        json!({"rev":"HEAD~1","mode":"hard"}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(fs::read_to_string(repo.join("b.txt")).unwrap(), "side\n");
    let (status, _) = call(
        f.app(),
        "POST",
        &at("reset"),
        json!({"rev":"HEAD~1","mode":"sideways"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(
        f.app(),
        "POST",
        &at("switch"),
        json!({"branch":picked,"detach":true}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        f.app(),
        "POST",
        &at("switch"),
        json!({"branch":"from-side","create":true,"start_point":"side~1"}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, picked) = call(f.app(), "POST", &at("cherry-pick"), json!({"rev":picked})).await;
    assert_eq!(status, StatusCode::OK, "{picked}");
    assert_eq!(fs::read_to_string(repo.join("b.txt")).unwrap(), "side\n");

    // A remote: add it, publish the branch, see it tracked, fetch and pull.
    let remote = f.path.join("remote.git");
    git(&f.path, &["init", "-q", "--bare", remote.to_str().unwrap()]);
    let (status, _) = call(
        f.app(),
        "POST",
        &at("remotes"),
        json!({"name":"origin","url":"ext::sh -c touch% /tmp/pwned"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, remotes) = call(
        f.app(),
        "POST",
        &at("remotes"),
        json!({"name":"origin","url":remote}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{remotes}");
    assert_eq!(remotes[0]["name"], "origin");
    let (status, pushed) = call(f.app(), "POST", &at("push"), json!({"set_upstream":true})).await;
    assert_eq!(status, StatusCode::OK, "{pushed}");
    let (_, current) = call(f.app(), "GET", &at("status"), Value::Null).await;
    assert_eq!(current["upstream"], "origin/from-side");
    let (_, branches) = call(f.app(), "GET", &at("branches"), Value::Null).await;
    assert!(
        branches["remote"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["name"] == "origin/from-side")
    );
    let local = branches["local"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == "from-side")
        .unwrap();
    assert_eq!(local["upstream"], "origin/from-side");
    git(&repo, &["reset", "-q", "--hard", "HEAD~1"]);
    let (status, _) = call(f.app(), "POST", &at("fetch"), json!({})).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, current) = call(f.app(), "GET", &at("status"), Value::Null).await;
    assert_eq!(current["behind"], 1);
    let (status, pulled) = call(f.app(), "POST", &at("pull"), json!({})).await;
    assert_eq!(status, StatusCode::OK, "{pulled}");
    let (_, current) = call(f.app(), "GET", &at("status"), Value::Null).await;
    assert!(current.get("behind").is_none());
    let (status, _) = call(
        f.app(),
        "POST",
        &at("push"),
        json!({"remote":"origin","branch":"from-side","delete":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, remotes) = call(
        f.app(),
        "POST",
        &at("remotes/remove"),
        json!({"name":"origin"}),
    )
    .await;
    assert_eq!(remotes, json!([]));
}
