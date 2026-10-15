use super::*;
use serde_json::{Value, json};

fn shown<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap()
}

#[test]
fn the_log_reads_each_record_and_its_decorations() {
    let raw = "abc\x1fp1 p2\x1fAda\x1fada@example.com\x1f1700000000\x1fHEAD -> main, tag: v1\x1fMerge it  \0\
               \nroot\x1f\x1fBob\x1fbob@example.com\x1fnot-a-date\x1f\x1fFirst\n\0";
    assert_eq!(
        shown(&parse_log(raw)),
        json!([
            {"hash":"abc","parents":["p1","p2"],"author":"Ada","email":"ada@example.com",
             "date":1700000000,"refs":["HEAD -> main","tag: v1"],"subject":"Merge it"},
            {"hash":"root","parents":[],"author":"Bob","email":"bob@example.com",
             "date":0,"refs":[],"subject":"First"},
        ])
    );
    // A record cut short is skipped rather than half read.
    assert!(parse_log("abc\x1fp1\x1fAda").is_empty());
    assert_eq!(decorations(""), Vec::<String>::new());
}

#[test]
fn changed_files_take_their_names_then_their_line_counts() {
    let names = "M\0src/a.rs\0R087\0old.rs\0new.rs\0D\0gone.txt\0M\0logo.png\0";
    let mut files = parse_name_status(names, false);
    // A rename names both sides after an empty path.
    apply_numstat(
        concat!(
            "3\t1\tsrc/a.rs\0",
            "2\t0\t\0old.rs\0new.rs\0",
            "-\t-\tlogo.png\0"
        ),
        &mut files,
    );
    assert_eq!(
        shown(&files),
        json!([
            {"status":"M","path":"src/a.rs","added":3,"removed":1},
            {"status":"R","path":"new.rs","original_path":"old.rs","added":2,"removed":0},
            {"status":"D","path":"gone.txt"},
            // A binary file counts `-`: no line counts.
            {"status":"M","path":"logo.png"},
        ])
    );
    // A stash's untracked files are all additions.
    assert_eq!(
        shown(&parse_name_status("M\0new.txt\0", true)),
        json!([{"status":"A","path":"new.txt","untracked":true}])
    );
}

#[test]
fn a_commit_is_diffed_against_its_first_parent_or_as_a_root() {
    assert_eq!(
        diff_args("abc", Some("p1"), &["--numstat"]),
        args(&[
            "diff",
            "p1",
            "abc",
            "-M",
            "--no-ext-diff",
            "--no-color",
            "--numstat"
        ])
    );
    assert_eq!(
        diff_args("abc", None, &[]),
        args(&[
            "diff-tree",
            "--root",
            "-r",
            "--no-commit-id",
            "abc",
            "-M",
            "--no-ext-diff",
            "--no-color"
        ])
    );
}

#[test]
fn remote_names_and_urls_are_checked() {
    for good in ["origin", "up-stream_2", "my.fork"] {
        assert!(valid_remote_name(good).is_ok(), "{good}");
    }
    for bad in ["", "-x", ".hidden", "a b", "a/b", &"x".repeat(101)] {
        assert!(valid_remote_name(bad).is_err(), "{bad}");
    }
    for good in [
        "https://example.com/repo.git",
        "git@example.com:me/repo.git",
        "../local",
    ] {
        assert!(valid_url(good).is_ok(), "{good}");
    }
    // A transport that runs a command, an option, or a control character.
    for bad in [
        "",
        "ext::sh -c touch% /tmp/x",
        "FD::3",
        "--upload-pack=x",
        "a\nb",
    ] {
        assert!(valid_url(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_path_is_matched_literally_and_git_output_is_joined() {
    assert_eq!(literal_glob("src/[id]/*.rs"), r"src/\[id\]/\*.rs");
    assert_eq!(literal_glob(r"a\b?"), r"a\\b\?");
    assert_eq!(said(b" conflict \n", b""), "conflict");
    assert_eq!(said(b"out", b"err\n"), "out\nerr");
    assert_eq!(said(b"", b"  "), "");
}
